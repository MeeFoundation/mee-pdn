//! Restart recovery at the runtime level: a runtime spawned on a shut-down
//! one's directory hosts what the hosted-identities record names, and
//! everything else re-derives from each identity's directory. An
//! in-process respawn proves the recovery logic — the record's commit
//! point, the re-derivation paths, a withdrawal during the outage — not a
//! process that exits; that half is the container stand.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, ShareMode,
    SpawnOptions, SyncService as _,
};
use pdn_types::EntryPath;
use test_utils::{eventually, ids};

mod common;
use common::{claims_on, establish_patiently, granted_patiently};

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
/// and the failure is injected where the directory would be made, the one
/// step between provisioning and hosting, which a full disk is the
/// product's reason to reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_fails_leaves_no_half_hosted_identity() -> Result<()> {
    let runtime = memory_rt().await?;
    let before = runtime.provisioned_identities_for_test().await?.len();

    runtime.fail_next_directory_create_for_test().await;
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

/// A node holding `ticket` and nothing else, pointed at `target`'s address:
/// the bare ticket identity the denials probe with. It holds the ticket for
/// [`PROBE`], an identity of its own, because every replica sits in one.
/// Its import fires a sync attempt now and every interval after.
async fn ticket_holder_dialing(
    target: &Runtime,
    issuer: pdn_types::PdnId,
    mut ticket: data_layer::DocTicket,
) -> Result<data_layer::SyncNode> {
    let probe = data_layer::SyncNode::spawn(data_layer::SpawnOptions {
        reconcile_interval: RECONCILE,
        ..data_layer::SpawnOptions::memory()
    })
    .await?;
    probe.provision_identity(PROBE).await?;
    ticket.nodes = vec![target.sync().dial_handle_for_test().await.addr()];
    probe.import_namespace_scoped(PROBE, issuer, ticket).await?;
    Ok(probe)
}

/// The identity the bare ticket identity acts as.
const PROBE: pdn_types::PdnId = ids::DAVE;

/// The hosted-identities record's file name, as the runtime writes it.
const RECORD: &str = "hosted-identities.json";

/// Left opaque: the scenarios move a line between two disks rather than
/// construct one.
fn record_lines(dir: &std::path::Path) -> Result<Vec<serde_json::Value>> {
    Ok(serde_json::from_slice(&std::fs::read(dir.join(RECORD))?)?)
}

/// Replace `dir`'s record with `lines`.
fn write_record_lines(dir: &std::path::Path, lines: &[serde_json::Value]) -> Result<()> {
    std::fs::write(dir.join(RECORD), serde_json::to_vec(lines)?)?;
    Ok(())
}

/// An identity created before the restart is hosted after it, with no peer
/// and no ceremony repeated. Denied: with the record's line removed, the
/// same directory hosts nothing; with the record unreadable, the start
/// fails naming the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_runtime_hosts_what_its_record_names() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let first = runtime_on(dir.path()).await?;
    let node_id = first.node_id();
    let alice = first.identity().create().await?;
    first
        .data()
        .write(alice, alice, &path, b"written before")
        .await?;
    first.shutdown().await?;
    drop(first);

    let second = runtime_on(dir.path()).await?;
    assert_eq!(second.node_id(), node_id, "the node id must survive");
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the recorded identity must be hosted again"
    );
    // The data namespace re-binds from the directory's `data` ticket on the
    // armer's first sweep; the payload is local.
    assert!(
        eventually(|| async {
            match second.data().read(alice, alice, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"written before".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the entry written before the restart must read back"
    );
    second.shutdown().await?;
    drop(second);

    // Denial (line removed).
    std::fs::write(dir.path().join(RECORD), b"[]")?;
    let third = runtime_on(dir.path()).await?;
    assert!(
        third.sync().hosted_identities().await?.is_empty(),
        "an empty record must host nothing"
    );
    assert!(
        third.data().read(alice, alice, &path).await.is_err(),
        "a read addressed to the unrecorded identity must be refused"
    );
    third.shutdown().await?;
    drop(third);

    // Denial (record unreadable): the start fails naming the file.
    std::fs::write(dir.path().join(RECORD), b"not json")?;
    let Err(err) = runtime_on(dir.path()).await else {
        anyhow::bail!("an unreadable record must stop the start");
    };
    assert!(
        format!("{err:#}").contains("hosted-identities.json"),
        "the refusal must name the record: {err:#}"
    );
    Ok(())
}

/// Several identities on one node each come back, each listing its own
/// connections only.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_each_recover_their_own_connections() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let host = runtime_on(dir.path()).await?;
    let bob_rt = memory_rt().await?;
    let carol_rt = memory_rt().await?;

    let at_work = host.identity().create().await?;
    let at_leisure = host.identity().create().await?;
    let bob = bob_rt.identity().create().await?;
    let carol = carol_rt.identity().create().await?;

    let invite = host.connections().invite(at_work, None).await?;
    establish_patiently(&bob_rt, bob, &host, at_work, invite).await?;
    let invite = host.connections().invite(at_leisure, None).await?;
    establish_patiently(&carol_rt, carol, &host, at_leisure, invite).await?;

    host.shutdown().await?;
    drop(host);
    let recovered = runtime_on(dir.path()).await?;

    let hosted = recovered.sync().hosted_identities().await?;
    assert!(
        hosted.contains(&at_work) && hosted.contains(&at_leisure) && hosted.len() == 2,
        "both identities must be hosted again: {hosted:?}"
    );
    let work_side = recovered.connections().list(at_work).await?;
    assert!(
        work_side.contains(&bob) && !work_side.contains(&carol),
        "the work persona must list its own connection and no other: {work_side:?}"
    );
    let leisure_side = recovered.connections().list(at_leisure).await?;
    assert!(
        leisure_side.contains(&carol) && !leisure_side.contains(&bob),
        "the leisure persona must list its own connection and no other: {leisure_side:?}"
    );
    recovered.shutdown().await?;
    Ok(())
}

/// Two identities of one node, granted a different claim each of one
/// issuer, come back after a restart reading each its own and neither
/// the other's: every identity is restored with stores of its own, its
/// directory opened there and its granted namespace imported there.
///
/// Denied: neither identity reads the claim granted to its co-located
/// sibling, before the restart or after it.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, both identities before and after the restart
async fn two_identities_recover_each_its_own_granted_claim() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let host = runtime_on(dir.path()).await?;
    let issuer_rt = memory_rt().await?;
    let issuer = issuer_rt.identity().create().await?;
    let at_work = host.identity().create().await?;
    let at_leisure = host.identity().create().await?;

    let work_claim = EntryPath::new("contact/email")?;
    let leisure_claim = EntryPath::new("contact/phone")?;
    issuer_rt
        .data()
        .write(issuer, issuer, &work_claim, b"issuer@example.org")
        .await?;
    issuer_rt
        .data()
        .write(issuer, issuer, &leisure_claim, b"+1-555-0100")
        .await?;

    for (identity, claim) in [(at_work, &work_claim), (at_leisure, &leisure_claim)] {
        let invite = issuer_rt.connections().invite(issuer, None).await?;
        establish_patiently(&host, identity, &issuer_rt, issuer, invite).await?;
        granted_patiently(
            &issuer_rt,
            issuer,
            &host,
            identity,
            issuer,
            claims_on(issuer, claim, false),
        )
        .await?;
    }

    // Each reads its own before the restart — the premise, not the
    // subject.
    for (identity, claim, payload) in [
        (at_work, &work_claim, b"issuer@example.org".as_slice()),
        (at_leisure, &leisure_claim, b"+1-555-0100".as_slice()),
    ] {
        assert!(
            eventually(|| async {
                match host.data().read(identity, issuer, claim).await {
                    Ok(value) => Ok(value.as_deref() == Some(payload)),
                    Err(_not_bound_yet) => Ok(false),
                }
            })
            .await?,
            "the granted claim never reached {identity} — the premise of this test"
        );
    }

    host.shutdown().await?;
    drop(host);
    let recovered = runtime_on(dir.path()).await?;

    // Allowed: each identity comes back holding its own replica.
    for (identity, claim, payload) in [
        (at_work, &work_claim, b"issuer@example.org".as_slice()),
        (at_leisure, &leisure_claim, b"+1-555-0100".as_slice()),
    ] {
        assert!(
            eventually(|| async {
                match recovered.data().read(identity, issuer, claim).await {
                    Ok(value) => Ok(value.as_deref() == Some(payload)),
                    Err(_not_rebound_yet) => Ok(false),
                }
            })
            .await?,
            "{identity} did not come back reading its own granted claim"
        );
    }

    // Denied: neither reads the other's claim, ordered after both came
    // back so the absence is the grant's doing and not a slow recovery.
    for (identity, other) in [(at_work, &leisure_claim), (at_leisure, &work_claim)] {
        assert_eq!(
            recovered.data().read(identity, issuer, other).await?,
            None,
            "{identity} read the claim granted to its co-located sibling"
        );
    }

    recovered.shutdown().await?;
    issuer_rt.shutdown().await?;
    Ok(())
}

/// The record writer at its edges: a create whose record replacement fails
/// (the directory made unwritable, the closest stand-in for a full disk)
/// fails whole and keeps the first identity hosted; the store set it
/// provisioned is hosted by nobody after a restart. A successful change
/// replaces the file, observed by its inode.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_record_write_fails_the_create_and_keeps_the_first() -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let dir = tempfile::tempdir()?;
    let record_path = dir.path().join(RECORD);

    let runtime = runtime_on(dir.path()).await?;
    let alice = runtime.identity().create().await?;
    let record_after_first = std::fs::read(&record_path)?;
    let inode_after_first = std::fs::metadata(&record_path)?.ino();
    let tracked_after_first = runtime.sync().tracked_doc_count(alice).await?;

    // The directory refuses new files, so staging the replacement fails
    // while the stores — already open, in writable subdirectories — keep
    // working.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))?;
    let refused = runtime.identity().create().await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    assert!(
        refused.is_err(),
        "a create whose record cannot be written must fail"
    );
    assert_eq!(
        std::fs::read(&record_path)?,
        record_after_first,
        "the failed replacement must leave the previous record intact"
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

    // A fresh inode, never an edit in place.
    let bob = runtime.identity().create().await?;
    assert_ne!(
        std::fs::metadata(&record_path)?.ino(),
        inode_after_first,
        "the record must be replaced by rename, not edited in place"
    );

    // The restart hosts exactly what the record names.
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

/// A withdrawal during an outage is honoured after the restart — the
/// evidence is the counterparty's replica, not the memo the restart
/// cleared. The re-grant imports again with no ceremony, and a withdrawal
/// after the restart removes exactly that binding. Denied throughout: a
/// bare identity of the read ticket, pointed at the restarted runtime by
/// hand, obtains nothing.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one outage, its denial and its re-grant in the same place
async fn a_withdrawal_during_an_outage_closes_the_replica() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let issuer_rt = memory_rt().await?;
    let audience_rt = runtime_on(dir.path()).await?;
    let issuer = issuer_rt.identity().create().await?;
    let audience = audience_rt.identity().create().await?;

    let invite = issuer_rt.connections().invite(issuer, None).await?;
    establish_patiently(&audience_rt, audience, &issuer_rt, issuer, invite).await?;
    issuer_rt
        .data()
        .write(issuer, issuer, &path, b"granted")
        .await?;
    common::granted_patiently(
        &issuer_rt,
        issuer,
        &audience_rt,
        audience,
        issuer,
        claims_on(issuer, &path, false),
    )
    .await?;
    assert!(
        eventually(|| async {
            match audience_rt.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"granted".as_slice())),
                Err(_not_bound_yet) => Ok(false),
            }
        })
        .await?,
        "the audience never read the granted entry before the stop"
    );

    // The ticket the denial presents, captured while the grant is live.
    let leaked_ticket = issuer_rt
        .data()
        .share(issuer, issuer, ShareMode::Read)
        .await?;

    // The outage, and the withdrawal inside it.
    audience_rt.shutdown().await?;
    drop(audience_rt);
    issuer_rt
        .connections()
        .withdraw_grant(issuer, audience, issuer)
        .await?;

    // The local grant record is stale — still live on this disk — so the
    // refusal is asserted only once the counterparty's replica has
    // demonstrably spoken and a sweep has run against that state.
    let recovered = runtime_on(dir.path()).await?;
    assert!(
        eventually(|| async {
            let devices = recovered
                .connections()
                .published_devices_of(audience, issuer)
                .await?;
            if devices.is_empty() {
                return Ok(false);
            }
            if !recovered
                .connections()
                .read_grants(audience, issuer)
                .await?
                .is_empty()
            {
                return Ok(false);
            }
            recovered
                .connections()
                .sweep_pair_now(audience, issuer)
                .await?;
            Ok(recovered
                .data()
                .read(audience, issuer, &path)
                .await
                .is_err())
        })
        .await?,
        "the withdrawal written during the outage must close the replica"
    );

    // Denial: the ticket identity, dialing the restarted runtime itself;
    // asserted below, after a proven wave.
    let probe = ticket_holder_dialing(&recovered, issuer, leaked_ticket).await?;

    // The re-grant: imported again with no ceremony.
    issuer_rt
        .data()
        .write(issuer, issuer, &path, b"re-granted")
        .await?;
    common::granted_patiently(
        &issuer_rt,
        issuer,
        &recovered,
        audience,
        issuer,
        claims_on(issuer, &path, false),
    )
    .await?;
    assert!(
        eventually(|| async {
            match recovered.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"re-granted".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the re-granted namespace must import again with no ceremony"
    );

    // Three more of the probe's intervals after the proven wave.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        probe.read(PROBE, issuer, &path).await?.is_none(),
        "a ticket identity with no grant must obtain nothing from the restarted node"
    );
    assert!(
        probe.list(PROBE, issuer, None).await?.is_empty(),
        "a ticket identity with no grant must not even list the namespace"
    );

    // The withdrawal with the runtime on removes the binding the re-import
    // recorded.
    issuer_rt
        .connections()
        .withdraw_grant(issuer, audience, issuer)
        .await?;
    assert!(
        eventually(|| async {
            Ok(recovered
                .data()
                .read(audience, issuer, &path)
                .await
                .is_err())
        })
        .await?,
        "a withdrawal after the restart must remove the re-imported binding"
    );

    probe.shutdown().await?;
    recovered.shutdown().await?;
    issuer_rt.shutdown().await?;
    Ok(())
}

/// A record line whose directory replica the store does not hold is
/// skipped: the start succeeds, that identity is not hosted, and every
/// healthy line beside it comes back. The commit precedes the rename, so
/// the state is arranged the only way left — a real record line carried
/// from one node's disk onto another's. The denials keep the skip from
/// being a shrug: the skipped identity is refused, not re-created, and the
/// record is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_whose_replica_is_absent_is_skipped_and_the_rest_comes_back() -> Result<()> {
    let path = EntryPath::new("contact/email")?;

    // A real record line, written by a real create on a disk this test
    // leaves behind.
    let elsewhere = tempfile::tempdir()?;
    let stranger_rt = runtime_on(elsewhere.path()).await?;
    let stranger = stranger_rt.identity().create().await?;
    stranger_rt.shutdown().await?;
    drop(stranger_rt);
    let stranger_line = record_lines(elsewhere.path())?;

    // The disk under test: one healthy identity, and the stranger's line
    // appended to its record.
    let dir = tempfile::tempdir()?;
    let first = runtime_on(dir.path()).await?;
    let alice = first.identity().create().await?;
    first
        .data()
        .write(alice, alice, &path, b"written before")
        .await?;
    first.shutdown().await?;
    drop(first);
    let mut lines = record_lines(dir.path())?;
    lines.extend(stranger_line);
    write_record_lines(dir.path(), &lines)?;
    let record_as_arranged = std::fs::read(dir.path().join(RECORD))?;

    let second = runtime_on(dir.path()).await?;
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the healthy line must come back and the absent one must be skipped"
    );
    assert!(
        eventually(|| async {
            match second.data().read(alice, alice, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"written before".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the healthy identity's entry must read back across the skip"
    );
    assert!(
        second.data().read(alice, stranger, &path).await.is_err(),
        "the skipped identity must be refused, not hosted from a fresh replica"
    );
    assert_eq!(
        std::fs::read(dir.path().join(RECORD))?,
        record_as_arranged,
        "the skip must leave the record as it was"
    );
    second.shutdown().await?;
    Ok(())
}

/// A start that fails after the stores are open (the unreadable record is
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

    // The record cannot be parsed, and the refusal comes after the stores
    // are open.
    let record = std::fs::read(dir.path().join(RECORD))?;
    std::fs::write(dir.path().join(RECORD), b"not json")?;
    assert!(
        runtime_on(dir.path()).await.is_err(),
        "an unreadable record must stop the start"
    );

    // The retry, on the same directory in the same process.
    std::fs::write(dir.path().join(RECORD), &record)?;
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

/// A connection and its grant come back after a restart from the durable
/// records alone — listed, readable, the granted entries readable once the
/// pair's first sweep has run — and an entry the issuer writes afterwards
/// arrives, so what came back is a live replica. Denied: a bare identity of
/// the read ticket pointed at the restarted runtime obtains nothing.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one restart, its liveness and its denial in the same place
async fn a_connection_and_its_live_grant_come_back() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let issuer_rt = memory_rt().await?;
    let audience_rt = runtime_on(dir.path()).await?;
    let issuer = issuer_rt.identity().create().await?;
    let audience = audience_rt.identity().create().await?;

    let invite = issuer_rt.connections().invite(issuer, None).await?;
    establish_patiently(&audience_rt, audience, &issuer_rt, issuer, invite).await?;
    issuer_rt
        .data()
        .write(issuer, issuer, &path, b"granted")
        .await?;
    common::granted_patiently(
        &issuer_rt,
        issuer,
        &audience_rt,
        audience,
        issuer,
        claims_on(issuer, &path, false),
    )
    .await?;
    assert!(
        eventually(|| async {
            match audience_rt.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"granted".as_slice())),
                Err(_not_bound_yet) => Ok(false),
            }
        })
        .await?,
        "the audience never read the granted entry before the restart"
    );

    // The ticket the denial presents, captured while the grant is live.
    let leaked_ticket = issuer_rt
        .data()
        .share(issuer, issuer, ShareMode::Read)
        .await?;

    // The outage; nothing withdrawn, nothing re-granted.
    audience_rt.shutdown().await?;
    drop(audience_rt);
    let recovered = runtime_on(dir.path()).await?;

    assert!(
        recovered
            .connections()
            .list(audience)
            .await?
            .contains(&issuer),
        "the connection must be listed again after the restart"
    );
    assert!(
        eventually(|| async {
            Ok(!recovered
                .connections()
                .read_grants(audience, issuer)
                .await?
                .is_empty())
        })
        .await?,
        "the grant must be readable from the pair after the restart"
    );
    assert!(
        eventually(|| async {
            match recovered.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"granted".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the granted namespace must read again after the restart"
    );

    // The probe, asserted below after a proven wave.
    let probe = ticket_holder_dialing(&recovered, issuer, leaked_ticket).await?;

    // A live replica, not the bytes the outage left. The rewrite stays
    // inside the granted claim.
    issuer_rt
        .data()
        .write(issuer, issuer, &path, b"after")
        .await?;
    assert!(
        eventually(|| async {
            match recovered.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"after".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "a rewrite after the restart must reach the audience over the recovered binding"
    );

    // Three more of the probe's intervals after the proven wave.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        probe.read(PROBE, issuer, &path).await?.is_none(),
        "a ticket identity with no grant must obtain nothing from the restarted node"
    );
    assert!(
        probe.list(PROBE, issuer, None).await?.is_empty(),
        "a ticket identity with no grant must not even list the namespace"
    );

    probe.shutdown().await?;
    recovered.shutdown().await?;
    issuer_rt.shutdown().await?;
    Ok(())
}

/// A grant published from a device that is then lost reaches the issuer's
/// other device from the audience's, the only live identity of the record;
/// without it the sibling would refuse the audience fail-closed while the
/// issuer believes the grant published. Denied: a bare identity of the read
/// ticket obtains nothing from the recovered device.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the loss and the recovery in one place
async fn a_grant_published_by_a_lost_device_reaches_the_sibling_from_the_audience() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let publisher_rt = memory_rt().await?;
    let sibling_rt = runtime_on(dir.path()).await?;
    let audience_rt = memory_rt().await?;

    let issuer = publisher_rt.identity().create().await?;
    common::link_patiently(&sibling_rt, &publisher_rt, issuer).await?;
    let audience = audience_rt.identity().create().await?;
    let invite = publisher_rt.connections().invite(issuer, None).await?;
    establish_patiently(&audience_rt, audience, &publisher_rt, issuer, invite).await?;
    publisher_rt
        .data()
        .write(issuer, issuer, &path, b"before")
        .await?;

    // The ticket the denial presents, minted while the publisher is up.
    let leaked_ticket = publisher_rt
        .data()
        .share(issuer, issuer, ShareMode::Read)
        .await?;

    // The pair is open on the sibling before it goes down: the connection
    // record alone leaves the pair's tickets payload-waiting, and the only
    // device holding those payloads is the publisher this scenario takes
    // away for good.
    assert!(
        eventually(|| async {
            let (own, _peer) = sibling_rt
                .connections()
                .pair_contacts(issuer, audience)
                .await?;
            Ok(!own.is_empty())
        })
        .await?,
        "the pair never opened on the sibling"
    );
    sibling_rt.shutdown().await?;
    drop(sibling_rt);

    // Published while the sibling is away and read by the audience.
    common::granted_patiently(
        &publisher_rt,
        issuer,
        &audience_rt,
        audience,
        issuer,
        claims_on(issuer, &path, false),
    )
    .await?;
    publisher_rt.shutdown().await?;

    let recovered = runtime_on(dir.path()).await?;
    assert!(
        eventually(|| async {
            Ok(recovered
                .connections()
                .read_own_grants(issuer, audience)
                .await?
                .is_some_and(|grant| grant.issuer == issuer))
        })
        .await?,
        "the grant record never reached the device that has to serve by it"
    );

    // Not merely present: the recovered device serves by it.
    recovered
        .data()
        .write(issuer, issuer, &path, b"after")
        .await?;
    assert!(
        eventually(|| async {
            match audience_rt.data().read(audience, issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"after".as_slice())),
                Err(_not_bound_yet) => Ok(false),
            }
        })
        .await?,
        "the audience never converged on the recovered device's write"
    );

    // The probe, after the proven wave and three of its own intervals.
    let probe = ticket_holder_dialing(&recovered, issuer, leaked_ticket).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        probe.read(PROBE, issuer, &path).await?.is_none(),
        "a ticket identity with no grant must obtain nothing from the recovered device"
    );
    assert!(
        probe.list(PROBE, issuer, None).await?.is_empty(),
        "a ticket identity with no grant must not even list the namespace"
    );

    probe.shutdown().await?;
    recovered.shutdown().await?;
    audience_rt.shutdown().await?;
    Ok(())
}

/// A link whose record cannot be written leaves nothing anywhere: the
/// identity's directory never names the device, because the confirmation
/// is written after the record, and a start on the directory the failed
/// link left hosts nothing from it. The injected failure is the directory
/// made unwritable; the retry with permissions back is what makes the
/// absence the failure's doing.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_cannot_be_recorded_leaves_nothing_on_the_identity() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let inviter = memory_rt().await?;
    let identity = inviter.identity().create().await?;
    // The store-level view of the directory.
    let (probe, directory) = common::link_probe(&inviter, identity).await?;

    let dir = tempfile::tempdir()?;
    let dialer = runtime_on(dir.path()).await?;
    let newcomer = dialer.node_id();

    // The directory refuses new files, so staging the record fails while
    // the stores carry the ceremony.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))?;
    let payload = inviter.identity().linking_invite(identity, None).await?;
    let refused = dialer
        .identity()
        .link(payload, Duration::from_secs(30))
        .await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    assert!(
        refused.is_err(),
        "a link whose record cannot be written must fail"
    );
    assert!(
        dialer.sync().hosted_identities().await?.is_empty(),
        "the failed link must leave nothing hosted"
    );

    // And a start on that directory brings none of it back: the record
    // names nothing, so whatever stores the link opened come back to
    // nobody.
    dialer.shutdown().await?;
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
        !directory.list_devices().await?.contains(&newcomer),
        "the failed link must not name this device in the identity's directory"
    );
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        !directory.list_devices().await?.contains(&newcomer),
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
        eventually(|| async { Ok(directory.list_devices().await?.contains(&newcomer)) }).await?,
        "the retried link must name this device in the identity's directory"
    );

    dialer.shutdown().await?;
    probe.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}

/// The order the failure above depends on: at the moment a link is about
/// to commit it has published nothing into the directory. Held there by a
/// pause, read from a device that already belongs to the identity, waiting
/// long enough for an earlier record to have replicated. A rollback undoes
/// what is local and cannot take back what replicated; committing first
/// keeps the second category empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_publishes_nothing_before_its_commit_point() -> Result<()> {
    let inviter = memory_rt().await?;
    let identity = inviter.identity().create().await?;
    let (probe, directory) = common::link_probe(&inviter, identity).await?;

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
    let published = directory.list_devices().await?;
    assert!(
        !published.contains(&newcomer),
        "a link must publish nothing into the directory before it commits: {published:?}"
    );
    assert!(
        published.contains(&probe.node_id()),
        "the reading device must be in the set it reads, or the read proves nothing"
    );

    pause.release();
    linking.await??;
    assert!(
        eventually(|| async { Ok(directory.list_devices().await?.contains(&newcomer)) }).await?,
        "the committed link must confirm the device afterwards"
    );

    dialer.shutdown().await?;
    probe.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}
