//! Restart recovery at the runtime level: a runtime spawned on the
//! directory of a shut-down one hosts what the hosted-identities record
//! names, and everything else re-derives from each identity's private
//! metadata directory — the data namespace from its `data` ticket, the
//! metadata pairs from their published tickets, the granted namespaces
//! from the counterparty's grant records.
//!
//! An in-process respawn cannot prove anything about a process that exits;
//! that half lives on the container stand. What it can prove is the
//! recovery logic itself: the record's commit point, the re-derivation
//! paths, and what a withdrawal during the outage does.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, ShareMode,
    SpawnOptions, SyncService as _,
};
use pdn_types::EntryPath;
use test_utils::eventually;

mod common;
use common::{claims_on, establish_patiently};

/// The suites' short reconcile cadence — the scenarios here wait on sweeps
/// and reconciliations, and the production 10s default would turn each
/// wait into pure sleep.
const RECONCILE: Duration = Duration::from_millis(500);

/// A runtime on memory, at the test cadence — the peers that stay up.
async fn memory_rt() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// A runtime on `dir`, at the test cadence — the node that restarts.
async fn runtime_on(dir: &std::path::Path) -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::on_directory(dir)
    })
    .await
}

/// The hosted-identities record's file name, as the runtime writes it.
const RECORD: &str = "hosted-identities.json";

/// The record's lines, left opaque: the scenarios below move a line
/// between two disks rather than construct one, so what a line says stays
/// the runtime's business.
fn record_lines(dir: &std::path::Path) -> Result<Vec<serde_json::Value>> {
    Ok(serde_json::from_slice(&std::fs::read(dir.join(RECORD))?)?)
}

/// Replace `dir`'s record with `lines`.
fn write_record_lines(dir: &std::path::Path, lines: &[serde_json::Value]) -> Result<()> {
    std::fs::write(dir.join(RECORD), serde_json::to_vec(lines)?)?;
    Ok(())
}

/// An identity created before the restart is hosted after it — same node
/// id, its entry readable with no peer running and no ceremony repeated.
///
/// The paired denials make the recovery the thing under test rather than a
/// coincidence: with the record's line removed, the same directory hosts
/// nothing and reads addressed to the identity are refused; with the
/// record unreadable, the start fails naming the file — never a healthy
/// start that hosts nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_runtime_hosts_what_its_record_names() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let first = runtime_on(dir.path()).await?;
    let node_id = first.node_id();
    let alice = first.identity().create().await?;
    first.data().write(alice, &path, b"written before").await?;
    first.shutdown().await?;
    drop(first);

    let second = runtime_on(dir.path()).await?;
    assert_eq!(second.node_id(), node_id, "the node id must survive");
    assert_eq!(
        second.sync().hosted_identities().await?,
        vec![alice],
        "the recorded identity must be hosted again"
    );
    // The data namespace re-binds from the directory's `data` ticket on
    // the armer's first sweep; the entry and its payload are local, so no
    // peer is needed.
    assert!(
        eventually(|| async {
            match second.data().read(alice, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"written before".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the entry written before the restart must read back"
    );
    second.shutdown().await?;
    drop(second);

    // Denial (line removed): what the record does not name is not hosted,
    // and reads addressed to it are refused — the recovery above cannot
    // have passed on something other than the record.
    std::fs::write(dir.path().join(RECORD), b"[]")?;
    let third = runtime_on(dir.path()).await?;
    assert!(
        third.sync().hosted_identities().await?.is_empty(),
        "an empty record must host nothing"
    );
    assert!(
        third.data().read(alice, &path).await.is_err(),
        "a read addressed to the unrecorded identity must be refused"
    );
    third.shutdown().await?;
    drop(third);

    // Denial (record unreadable): the start fails naming the file, rather
    // than coming up healthy with nothing hosted.
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

/// Several identities on one node each come back: two identities with a
/// connection each, restarted, each hosting its own and listing its own
/// connections only — which is what makes "each identity comes back with
/// its own connections" a property rather than a coincidence of testing
/// one.
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

/// The record writer at its edges: a create whose record replacement fails
/// — the directory made unwritable, the closest injectable stand-in for a
/// full disk — fails whole and keeps the first identity hosted, from a
/// previous record left intact; the store set it provisioned is hosted by
/// nobody after a restart. A successful change replaces the file rather
/// than editing it in place, observed by its inode.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_record_write_fails_the_create_and_keeps_the_first() -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let dir = tempfile::tempdir()?;
    let record_path = dir.path().join(RECORD);

    let runtime = runtime_on(dir.path()).await?;
    let alice = runtime.identity().create().await?;
    let record_after_first = std::fs::read(&record_path)?;
    let inode_after_first = std::fs::metadata(&record_path)?.ino();

    // The injected failure: the directory refuses new files, so staging
    // the replacement fails while the stores — files already open, in
    // writable subdirectories — keep working. The create provisions its
    // store set and dies exactly at the commit point.
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

    // A later successful change replaces the file whole — a fresh inode,
    // never an edit in place.
    let bob = runtime.identity().create().await?;
    assert_ne!(
        std::fs::metadata(&record_path)?.ino(),
        inode_after_first,
        "the record must be replaced by rename, not edited in place"
    );

    // The restart hosts exactly what the record names: the two committed
    // identities, and nothing from the interrupted provisioning — its
    // replicas sit in the store with no record pointing at them, never
    // registered and never served.
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

/// A withdrawal that lands during an outage is honoured after the restart:
/// the granted namespace stops being readable and its issuer resolves to
/// nothing — the evidence is the counterparty's replica, not the memo the
/// restart cleared. The re-grant over the same claim imports again with no
/// ceremony, and a withdrawal after the restart removes exactly the
/// binding that re-import recorded.
///
/// The tightest denial rides the same scenario: a bare node holding the
/// namespace's read ticket — pointed at the restarted runtime by hand, the
/// access-control negative control — obtains nothing across every wave the
/// scenario proves, while the granted audience reads.
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
    issuer_rt.data().write(issuer, &path, b"granted").await?;
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
            match audience_rt.data().read(issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"granted".as_slice())),
                Err(_not_bound_yet) => Ok(false),
            }
        })
        .await?,
        "the audience never read the granted entry before the stop"
    );

    // The ticket the denial below presents, captured while the grant is
    // live: a real read ticket to the issuer's namespace, with no grant
    // behind its holder.
    let leaked_ticket = issuer_rt.data().share(issuer, ShareMode::Read).await?;

    // The outage, and the withdrawal inside it.
    audience_rt.shutdown().await?;
    drop(audience_rt);
    issuer_rt
        .connections()
        .withdraw_grant(issuer, audience, issuer)
        .await?;

    // The restart reconnects from durable tickets alone. The local grant
    // record is stale — still live on this disk — so the refusal is
    // asserted only once the counterparty's replica has demonstrably
    // spoken: devices synced, the grant record tombstoned, and a sweep run
    // against that state.
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
            Ok(recovered.data().read(issuer, &path).await.is_err())
        })
        .await?,
        "the withdrawal written during the outage must close the replica"
    );

    // Denial: a holder of the namespace's ticket with no grant, dialing
    // the restarted runtime itself. Its import fires a sync attempt now
    // and every reconcile interval after; the assertion waits below, after
    // a proven wave.
    let probe = data_layer::SyncNode::spawn(data_layer::SpawnOptions {
        reconcile_interval: RECONCILE,
        ..data_layer::SpawnOptions::memory()
    })
    .await?;
    let mut ticket = leaked_ticket;
    ticket.nodes = vec![recovered.sync().dial_handle_for_test().await.addr()];
    probe.import_namespace_scoped(issuer, ticket).await?;

    // The re-grant over the same claim: imported again with no ceremony —
    // no invite minted, no establishment run since the restart.
    issuer_rt.data().write(issuer, &path, b"re-granted").await?;
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
            match recovered.data().read(issuer, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"re-granted".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the re-granted namespace must import again with no ceremony"
    );

    // The audience's read above is the proven wave; three more of the
    // probe's own intervals mean "it tried repeatedly and was refused" is
    // what keeps this green, not a poll that outran its first dial.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        probe.read(issuer, &path).await?.is_none(),
        "a ticket holder with no grant must obtain nothing from the restarted node"
    );
    assert!(
        probe.list(issuer, None).await?.is_empty(),
        "a ticket holder with no grant must not even list the namespace"
    );

    // The binding the re-import recorded is the one this removes: the
    // withdrawal with the runtime on, closing the loop after recovery.
    issuer_rt
        .connections()
        .withdraw_grant(issuer, audience, issuer)
        .await?;
    assert!(
        eventually(|| async { Ok(recovered.data().read(issuer, &path).await.is_err()) }).await?,
        "a withdrawal after the restart must remove the re-imported binding"
    );

    probe.shutdown().await?;
    recovered.shutdown().await?;
    issuer_rt.shutdown().await?;
    Ok(())
}

/// A record line whose directory replica the store does not hold does not
/// take the node down with it: the start succeeds, that identity is not
/// hosted, and every healthy line beside it comes back. Such a line is
/// what a process killed between the record's rename and the store's
/// commit used to leave; the commit now precedes the rename, so the state
/// is arranged the only way left — by carrying a real record line from one
/// node's disk onto another's, where the named replica has never been.
///
/// The denials keep the skip from being a shrug: the skipped identity is
/// refused, not silently re-created, and the record is left as it was, so
/// the line is still there to be read by whoever ends the hosting.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_whose_replica_is_absent_is_skipped_and_the_rest_comes_back() -> Result<()> {
    let path = EntryPath::new("contact/email")?;

    // The stranger's line: a real record, written by a real create, on a
    // disk this test then leaves behind.
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
    first.data().write(alice, &path, b"written before").await?;
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
            match second.data().read(alice, &path).await {
                Ok(payload) => Ok(payload.as_deref() == Some(b"written before".as_slice())),
                Err(_not_rebound_yet) => Ok(false),
            }
        })
        .await?,
        "the healthy identity's entry must read back across the skip"
    );
    assert!(
        second.data().read(stranger, &path).await.is_err(),
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
