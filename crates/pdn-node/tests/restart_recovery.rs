//! Restart recovery at the runtime level: a runtime spawned on a shut-down
//! one's directory hosts the identities whose subdirectory records their
//! hosting, and everything else re-derives from each identity's PMS.
//! An in-process respawn proves the recovery logic — the record's commit
//! point, the re-derivation paths, a withdrawal during the outage — not a
//! process that exits; that half is the container stand.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{IdentityService as _, PodsService as _, Runtime, SpawnOptions, SyncService as _};
use test_utils::eventually;

mod common;
use common::{link_patiently, link_probe};

const RECONCILE: Duration = Duration::from_millis(500);

/// The peers that stay up.
async fn memory_rt() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// A create that fails after the identity's half of the node is up leaves
/// nothing of it: the node hosts no more identities than before, and the
/// next create still works. Provisioning is the first act with something
/// to undo — an actor thread, an open store, an entry in the hosted set —
/// and the failure is injected where the PMS would be made, a step
/// between provisioning and hosting, which a full disk is the product's
/// reason to reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_fails_leaves_no_half_hosted_identity() -> Result<()> {
    let runtime = memory_rt().await?;
    let before = runtime.provisioned_identities_for_test().await?.len();

    runtime.fail_next_pms_create_for_test().await;
    assert!(
        runtime.identity().create().await.is_err(),
        "the injected failure did not fail the create"
    );
    // The node's own halves, not the runtime's hosted set: the failure
    // lands between the two, so only the first shows what was left.
    assert_eq!(
        runtime.provisioned_identities_for_test().await?.len(),
        before,
        "the failed create left the identity's half of the node standing"
    );

    // The node is not poisoned by the attempt: the next create goes through.
    let created = runtime.identity().create().await?;
    assert!(
        runtime.sync().hosted_identities().await?.contains(&created),
        "the create after the failed one did not host its identity"
    );
    assert_eq!(
        runtime.provisioned_identities_for_test().await?.len(),
        before + 1,
        "the node holds a half it does not host"
    );

    runtime.shutdown().await?;
    Ok(())
}

/// The node that restarts.
async fn runtime_on(dir: &std::path::Path) -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::on_directory(dir)
    })
    .await
}

/// Where `identity`'s subdirectory lands under `dir`.
fn subdirectory_of(dir: &std::path::Path, identity: pdn_types::PdnId) -> std::path::PathBuf {
    dir.join("identities").join(identity.to_string())
}

/// Where `identity`'s hosting record lands under `dir`, as the node writes
/// it.
fn record_of(dir: &std::path::Path, identity: pdn_types::PdnId) -> std::path::PathBuf {
    subdirectory_of(dir, identity).join("pms")
}

/// Copy the files of one identity's subdirectory — it holds no others.
fn copy_subdirectory(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// `identity`'s pod of one and the claim placed in it.
async fn pod_with_claim(
    runtime: &Runtime,
    identity: pdn_types::PdnId,
) -> Result<(pdn_types::PodId, pdn_types::RecordRef)> {
    let pod = runtime.pods().create(identity).await?;
    let record = runtime
        .pods()
        .put_record(
            identity,
            pod,
            pdn_types::RecordKind::Claim,
            b"written before",
        )
        .await?;
    Ok((pod, record))
}

/// Whether `identity` on `runtime` comes to read the claim
/// [`pod_with_claim`] placed, its pod opened again or not yet.
async fn reads_claim(
    runtime: &Runtime,
    identity: pdn_types::PdnId,
    pod: pdn_types::PodId,
    record: pdn_types::RecordRef,
) -> Result<bool> {
    eventually(|| async {
        match runtime.pods().read(identity, pod, record).await {
            Ok(payload) => Ok(payload.as_deref() == Some(b"written before".as_slice())),
            Err(_not_reopened_yet) => Ok(false),
        }
    })
    .await
}

/// An identity created before the restart is hosted after it, with no peer
/// and no ceremony repeated. Denied: with its hosting record removed, the
/// same directory hosts nothing; with the record unreadable, the start
/// fails naming the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_runtime_hosts_what_its_record_names() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = runtime_on(dir.path()).await?;
    let node_id = first.node_id();
    let alice = first.identity().create().await?;
    let (pod, record) = pod_with_claim(&first, alice).await?;
    first.shutdown().await?;
    drop(first);

    let second = runtime_on(dir.path()).await?;
    assert_eq!(second.node_id(), node_id, "the node id must survive");
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the recorded identity must be hosted again"
    );
    // The pod reopens from the PMS's tickets on the armer's first sweep;
    // the payload is local.
    assert!(
        reads_claim(&second, alice, pod, record).await?,
        "the record placed before the restart must read back"
    );
    second.shutdown().await?;
    drop(second);

    // Denial (record removed).
    std::fs::remove_file(record_of(dir.path(), alice))?;
    let third = runtime_on(dir.path()).await?;
    assert!(
        third.sync().hosted_identities().await?.is_empty(),
        "a directory with no hosting record must host nothing"
    );
    assert!(
        third.pods().read(alice, pod, record).await.is_err(),
        "a read addressed to the unrecorded identity must be refused"
    );
    third.shutdown().await?;
    drop(third);

    // Denial (record unreadable): the start fails naming the file.
    let record = record_of(dir.path(), alice);
    std::fs::write(&record, b"not a namespace")?;
    let Err(err) = runtime_on(dir.path()).await else {
        anyhow::bail!("an unreadable record must stop the start");
    };
    assert!(
        format!("{err:#}").contains(&record.display().to_string()),
        "the refusal must name the record: {err:#}"
    );
    Ok(())
}

/// Several identities on one node each come back, each holding its own
/// pods only, reopened by the armer.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_each_recover_their_own_pods() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let host = runtime_on(dir.path()).await?;
    let bob_rt = memory_rt().await?;
    let carol_rt = memory_rt().await?;

    let at_work = host.identity().create().await?;
    let at_leisure = host.identity().create().await?;
    let bob = bob_rt.identity().create().await?;
    let carol = carol_rt.identity().create().await?;

    let work_pod = bob_rt.pods().create(bob).await?;
    let invite = bob_rt.pods().invite(bob, work_pod, None).await?;
    host.pods().join(at_work, invite).await?;
    let leisure_pod = carol_rt.pods().create(carol).await?;
    let invite = carol_rt.pods().invite(carol, leisure_pod, None).await?;
    host.pods().join(at_leisure, invite).await?;

    host.shutdown().await?;
    drop(host);
    let recovered = runtime_on(dir.path()).await?;

    let hosted = recovered.sync().hosted_identities().await?;
    assert!(
        hosted.contains(&at_work) && hosted.contains(&at_leisure) && hosted.len() == 2,
        "both identities must be hosted again: {hosted:?}"
    );
    for (identity, own, other, with) in [
        (at_work, work_pod, leisure_pod, bob),
        (at_leisure, leisure_pod, work_pod, carol),
    ] {
        let listed: Vec<_> = recovered
            .pods()
            .list(identity)
            .await?
            .into_iter()
            .map(|info| info.id)
            .collect();
        assert_eq!(
            listed,
            vec![own],
            "an identity must list its own pod and no other"
        );
        assert!(
            eventually(|| async {
                Ok(recovered
                    .pods()
                    .members(identity, own)
                    .await
                    .is_ok_and(|members| members.iter().any(|member| member.id == with)))
            })
            .await?,
            "an identity's pod did not reopen after the restart"
        );
        assert!(
            recovered.pods().members(identity, other).await.is_err(),
            "an identity reached its co-located sibling's pod"
        );
    }
    recovered.shutdown().await?;
    bob_rt.shutdown().await?;
    carol_rt.shutdown().await?;
    Ok(())
}

/// A create whose hosting record is refused at the commit point fails
/// whole and keeps the first identity hosted, and the store set it
/// provisioned is hosted by nobody after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_hosting_record_fails_the_create_and_keeps_the_first() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime = runtime_on(dir.path()).await?;
    let alice = runtime.identity().create().await?;
    let tracked_after_first = runtime.sync().tracked_doc_count(alice).await?;

    runtime.fail_next_hosting_record_for_test().await;
    assert!(
        runtime.identity().create().await.is_err(),
        "a create whose hosting record is refused must fail"
    );
    assert_eq!(
        runtime.sync().hosted_identities().await?,
        vec![alice],
        "the failed create must not disturb the hosted set"
    );
    // And nothing is left in the running node: the replicas provisioned
    // before the commit point are gone, rather than reconciled for the rest
    // of the process's life.
    assert_eq!(
        runtime.sync().tracked_doc_count(alice).await?,
        tracked_after_first,
        "the failed create must leave no replica tracked in the running node"
    );
    let bob = runtime.identity().create().await?;

    // The restart hosts exactly the identities whose commit went through.
    runtime.shutdown().await?;
    drop(runtime);
    let recovered = runtime_on(dir.path()).await?;
    let hosted = recovered.sync().hosted_identities().await?;
    assert!(
        hosted.contains(&alice) && hosted.contains(&bob) && hosted.len() == 2,
        "recovery must host the recorded identities and nothing else: {hosted:?}"
    );
    recovered.shutdown().await?;
    Ok(())
}

/// A hosting record whose PMS replica the store does not hold is
/// skipped: the start succeeds, that identity is not hosted, and every
/// healthy identity beside it comes back — whether the replica store is
/// gone from the record's subdirectory or holds no such replica. The
/// commit flushes the replicas before it writes the record, so the state
/// is arranged the only way left: subdirectories carried from another
/// node's disk. The denials keep the skip from being a shrug: a skipped
/// identity is refused, not re-created, a store that is gone stays gone,
/// and a create after the start leaves both records as they were.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_whose_replica_is_absent_is_skipped_and_the_rest_comes_back() -> Result<()> {
    // Real subdirectories, written by real creates on a disk this test
    // leaves behind.
    let elsewhere = tempfile::tempdir()?;
    let other_rt = runtime_on(elsewhere.path()).await?;
    let storeless = other_rt.identity().create().await?;
    let mismatched = other_rt.identity().create().await?;
    other_rt.shutdown().await?;
    drop(other_rt);

    let dir = tempfile::tempdir()?;
    let first = runtime_on(dir.path()).await?;
    let alice = first.identity().create().await?;
    let (pod, record) = pod_with_claim(&first, alice).await?;
    first.shutdown().await?;
    drop(first);

    // One record without its replica store, and one whose store holds a
    // replica other than the one its record names.
    std::fs::create_dir_all(subdirectory_of(dir.path(), storeless))?;
    std::fs::copy(
        record_of(elsewhere.path(), storeless),
        record_of(dir.path(), storeless),
    )?;
    copy_subdirectory(
        &subdirectory_of(elsewhere.path(), mismatched),
        &subdirectory_of(dir.path(), mismatched),
    )?;
    std::fs::copy(
        record_of(elsewhere.path(), storeless),
        record_of(dir.path(), mismatched),
    )?;
    let records_as_arranged = [
        std::fs::read(record_of(dir.path(), storeless))?,
        std::fs::read(record_of(dir.path(), mismatched))?,
    ];

    let second = runtime_on(dir.path()).await?;
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the healthy identity must come back and both others must be skipped"
    );
    assert!(
        reads_claim(&second, alice, pod, record).await?,
        "the healthy identity's record must read back across the skip"
    );
    for skipped in [storeless, mismatched] {
        assert!(
            second.pods().list(skipped).await.is_err(),
            "a skipped identity must be refused, not hosted from a fresh replica"
        );
    }
    assert!(
        !subdirectory_of(dir.path(), storeless)
            .join("docs.redb")
            .exists(),
        "the skip must not open a store where the record's store is gone"
    );

    // A create after the start leaves the skipped records where they were.
    let _carol = second.identity().create().await?;
    assert_eq!(
        [
            std::fs::read(record_of(dir.path(), storeless))?,
            std::fs::read(record_of(dir.path(), mismatched))?,
        ],
        records_as_arranged,
        "the skip and the create after it must leave the skipped records as they were"
    );
    second.shutdown().await?;
    Ok(())
}

/// A start that fails once the node is up (an unreadable hosting record is
/// raised after the node exists) leaves the directory reusable in the same
/// process. The wait is bounded on purpose: without the shutdown on the
/// failing path the retry hangs rather than fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_start_leaves_the_directory_reusable() -> Result<()> {
    const RETRY_BUDGET: Duration = Duration::from_secs(30);

    let dir = tempfile::tempdir()?;
    let first = runtime_on(dir.path()).await?;
    let alice = first.identity().create().await?;
    first.shutdown().await?;
    drop(first);

    // The record cannot be parsed, and the refusal comes after the node is
    // up.
    let record_path = record_of(dir.path(), alice);
    let record = std::fs::read(&record_path)?;
    std::fs::write(&record_path, b"not a namespace")?;
    assert!(
        runtime_on(dir.path()).await.is_err(),
        "an unreadable record must stop the start"
    );

    // The retry, on the same directory in the same process.
    std::fs::write(&record_path, &record)?;
    let retried = tokio::time::timeout(RETRY_BUDGET, runtime_on(dir.path()))
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!("the retry never returned: the failed start left its stores open")
        })??;
    assert_eq!(
        retried.sync().hosted_identities().await?,
        vec![alice],
        "the retry must host what the record names"
    );
    retried.shutdown().await?;
    Ok(())
}

/// A link whose record cannot be written leaves nothing anywhere: the
/// identity's PMS never names the device, because the confirmation
/// is written after the record, and a start on the directory the failed
/// link left hosts nothing from it. The injected failure is the identity's
/// subdirectory made unwritable while a pause holds the link at its commit
/// point; the retry with permissions back is what makes the absence the
/// failure's doing.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_cannot_be_recorded_leaves_nothing_on_the_identity() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let inviter = memory_rt().await?;
    let identity = inviter.identity().create().await?;
    // The store-level view of the PMS.
    let (probe, pms) = common::link_probe(&inviter, identity).await?;

    let dir = tempfile::tempdir()?;
    let dialer = std::sync::Arc::new(runtime_on(dir.path()).await?);
    let newcomer = dialer.node_id();
    let pause = dialer.pause_next_link_before_commit().await;

    let payload = inviter.identity().linking_invite(identity, None).await?;
    let linking = {
        let dialer = std::sync::Arc::clone(&dialer);
        tokio::spawn(async move {
            dialer
                .identity()
                .link(payload, Duration::from_secs(30))
                .await
        })
    };
    // The subdirectory refuses new files, so staging the record fails while
    // the stores the ceremony opened keep working.
    pause.wait_until_reached().await;
    let subdirectory = dir.path().join("identities").join(identity.to_string());
    std::fs::set_permissions(&subdirectory, std::fs::Permissions::from_mode(0o500))?;
    pause.release();
    let refused = linking.await?;
    std::fs::set_permissions(&subdirectory, std::fs::Permissions::from_mode(0o700))?;
    assert!(
        refused.is_err(),
        "a link whose record cannot be written must fail"
    );
    assert!(
        dialer.sync().hosted_identities().await?.is_empty(),
        "the failed link must leave nothing hosted"
    );

    // And a start on that directory brings none of it back: no record
    // names the identity, so whatever stores the link opened come back to
    // nobody.
    dialer.shutdown().await?;
    drop(dialer);
    let dialer = runtime_on(dir.path()).await?;
    assert!(
        dialer.sync().hosted_identities().await?.is_empty(),
        "a start on the directory a failed link left must host nothing from it"
    );
    assert_eq!(
        dialer.node_id(),
        newcomer,
        "the restart must be the same node, or the assertions below name another device"
    );

    // Not late either: the sweep that repeats a confirmation runs only for
    // a hosted identity. The order itself is asserted below.
    assert!(
        !pms.list_devices().await?.contains(&newcomer),
        "the failed link must not name this device in the identity's PMS"
    );
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        !pms.list_devices().await?.contains(&newcomer),
        "the failed link's device record must not arrive later either"
    );

    // The retry: the record is written, the confirmation follows.
    common::link_patiently(&dialer, &inviter, identity).await?;
    assert_eq!(
        dialer.sync().hosted_identities().await?,
        vec![identity],
        "the retried link must host the identity"
    );
    assert!(
        eventually(|| async { Ok(pms.list_devices().await?.contains(&newcomer)) }).await?,
        "the retried link must name this device in the identity's PMS"
    );

    dialer.shutdown().await?;
    probe.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}

/// The order the failure above depends on: at the moment a link is about
/// to commit it has published nothing into the PMS. Held there by a
/// pause, read from a device that already belongs to the identity, waiting
/// long enough for an earlier record to have replicated. A rollback undoes
/// what is local and cannot take back what replicated; committing first
/// keeps the second category empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_publishes_nothing_before_its_commit_point() -> Result<()> {
    let inviter = memory_rt().await?;
    let identity = inviter.identity().create().await?;
    let (probe, pms) = common::link_probe(&inviter, identity).await?;

    let dialer = std::sync::Arc::new(memory_rt().await?);
    let newcomer = dialer.node_id();
    let pause = dialer.pause_next_link_before_commit().await;

    let payload = inviter.identity().linking_invite(identity, None).await?;
    let linking = {
        let dialer = std::sync::Arc::clone(&dialer);
        tokio::spawn(async move {
            dialer
                .identity()
                .link(payload, Duration::from_secs(30))
                .await
        })
    };

    pause.wait_until_reached().await;
    // Long enough that a record written before the pause would have reached
    // this replica: the ceremony's own catch-up ran over the same path, and
    // three intervals follow.
    tokio::time::sleep(RECONCILE * 3).await;
    let published = pms.list_devices().await?;
    assert!(
        !published.contains(&newcomer),
        "a link must publish nothing into the PMS before it commits: {published:?}"
    );
    assert!(
        published.contains(&probe.node_id()),
        "the reading device must be in the set it reads, or the read proves nothing"
    );

    pause.release();
    linking.await??;
    assert!(
        eventually(|| async { Ok(pms.list_devices().await?.contains(&newcomer)) }).await?,
        "the committed link must confirm the device afterwards"
    );

    dialer.shutdown().await?;
    probe.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}

/// A runtime dropped without `shutdown` while the process goes on lets go
/// of its directory: a runtime spawned on it again in the same process
/// hosts what the first one hosted. The release runs on the dropped
/// runtime's own tasks, so a spawn meeting a store still held fails and is
/// retried within a budget; each attempt is bounded, so a spawn that hangs
/// costs one attempt rather than the run.
#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_dropped_without_shutdown_releases_its_directory() -> Result<()> {
    const RELEASE_BUDGET: Duration = Duration::from_secs(20);
    const ATTEMPT_BUDGET: Duration = Duration::from_secs(3);

    let dir = tempfile::tempdir()?;
    let first = runtime_on(dir.path()).await?;
    let alice = first.identity().create().await?;
    drop(first);

    let deadline = std::time::Instant::now() + RELEASE_BUDGET;
    let second = loop {
        if let Ok(Ok(runtime)) = tokio::time::timeout(ATTEMPT_BUDGET, runtime_on(dir.path())).await
        {
            break runtime;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the dropped runtime never released its directory"
        );
        tokio::time::sleep(RECONCILE).await;
    };
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the respawned runtime must host what the dropped one hosted"
    );
    second.shutdown().await?;
    Ok(())
}

/// A device that linked before a restart is still a device of the identity
/// after it, under the node id it registered with: a device linked into the
/// restarted runtime afterwards reads both devices out of the identity's
/// PMS.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_linked_before_the_restart_is_still_a_device() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = memory_rt().await?;
    let x = first.identity().create().await?;
    let linked = runtime_on(dir.path()).await?;
    link_patiently(&linked, &first, x).await?;
    let linked_id = linked.node_id();
    linked.shutdown().await?;
    drop(linked);

    let restarted = runtime_on(dir.path()).await?;
    assert_eq!(restarted.node_id(), linked_id, "the node id must survive");
    let (probe_node, probe_pms) = link_probe(&restarted, x).await?;
    assert!(
        test_utils::wait_devices(&probe_pms, &[first.node_id(), linked_id]).await?,
        "a device linked before the restart is no device after it"
    );

    probe_node.shutdown().await?;
    restarted.shutdown().await?;
    first.shutdown().await?;
    Ok(())
}

/// An identity whose create returned is on disk whole: a copy of the
/// directory taken at once comes back hosting it, with its identity key.
///
/// The copy stands in for a kill, holding what was on disk at that moment
/// and nothing a stop would write. Without the commit point's flush, the
/// key's payload sits in the blob store's write batch for up to a second
/// after the hosting record is on disk. Denied: a runtime on an empty
/// directory hosts nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_identity_whose_create_returned_is_on_disk_whole() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime = runtime_on(dir.path()).await?;
    let alice = runtime.identity().create().await?;
    let copy = tempfile::tempdir()?;
    test_utils::crash_copy(dir.path(), copy.path())?;
    let key = runtime.identity_public_key_for_test(alice).await?;
    runtime.shutdown().await?;
    assert!(key.is_some(), "the identity's own key must be readable");

    let recovered = runtime_on(copy.path()).await?;
    assert_eq!(
        recovered.sync().hosted_identities().await?,
        vec![alice],
        "the copy must host the identity whose create returned"
    );
    assert_eq!(
        recovered.identity_public_key_for_test(alice).await?,
        key,
        "the copy must hold the identity's key"
    );
    assert_eq!(
        recovered
            .own_entries_without_payload_for_test(alice)
            .await?,
        0,
        "the copy holds this device's entries without their payloads"
    );
    recovered.shutdown().await?;
    // Denied: nothing comes back from a directory that never held it.
    let empty = tempfile::tempdir()?;
    let fresh = runtime_on(empty.path()).await?;
    assert_eq!(fresh.sync().hosted_identities().await?, []);
    fresh.shutdown().await?;
    Ok(())
}

/// A pod is never held without its tickets: a copy of the directory taken
/// once the PMS's record of a created pod is on disk opens the pod.
///
/// The list reads the PMS, and that read commits the replica store's write
/// batch while the blob store's stays open: an order the stores' own
/// commits reach within their settle window. Without the flush before the
/// record, the copy then holds the record and the tickets' entries but not
/// their payloads, and the pod is listed and never opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_pod_whose_record_is_on_disk_opens_from_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime = runtime_on(dir.path()).await?;
    let alice = runtime.identity().create().await?;
    let pod = runtime.pods().create(alice).await?;
    assert!(
        runtime
            .pods()
            .list(alice)
            .await?
            .iter()
            .any(|info| info.id == pod),
        "the created pod must be listed"
    );
    let copy = tempfile::tempdir()?;
    test_utils::crash_copy(dir.path(), copy.path())?;
    runtime.shutdown().await?;

    let recovered = runtime_on(copy.path()).await?;
    let opened = test_utils::eventually(|| async {
        Ok(recovered
            .pods()
            .members(alice, pod)
            .await
            .is_ok_and(|members| {
                members
                    .iter()
                    .any(|member| member.id == alice && member.owner)
            }))
    })
    .await?;
    assert!(opened, "the copy must open the pod its record names");
    assert_eq!(
        recovered
            .own_entries_without_payload_for_test(alice)
            .await?,
        0,
        "the copy holds this device's entries without their payloads"
    );
    recovered.shutdown().await?;
    Ok(())
}
