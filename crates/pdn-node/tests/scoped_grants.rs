//! The scoped-grant flow end to end through the runtime services:
//! establishment arms both sides' classification, a scoped grant crosses
//! the metadata pair, the granted namespace imports scoped, and
//! capability-filtered reconciliation delivers exactly the granted
//! subset — with the paired denials of
//! `code-practices/access-control-tests.md` probed in the same place: the
//! outsider (no connection, no ticket — refused as unknown), the holder of
//! the replica's leaked ticket without a grant (obtains nothing), the
//! existence-hidden withheld claims, and the read-only holder's refused
//! write.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    claim_id_of, ConnectionsService as _, DataService as _, IdentityService as _, NonEmpty,
    Runtime, ScopedPeerGrant, SpawnOptions, UnknownIssuer,
};
use pdn_types::EntryPath;
use test_utils::eventually;

mod common;
use common::establish_patiently;

/// The reconcile cadence of this scenario — the ticket-holder denial below
/// is "it retried over several intervals and was refused", made cheap by
/// injecting a sub-second interval.
const RECONCILE: Duration = Duration::from_millis(500);

/// Spawn a runtime with the test's short reconcile cadence.
async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn_with(SpawnOptions {
        reconcile_interval: RECONCILE,
    })
    .await
}

/// Poll until the peer's scoped grant for `issuer` is readable.
async fn scoped_grant_patiently(
    receives: &Runtime,
    receives_id: pdn_types::PdnId,
    gives_id: pdn_types::PdnId,
    issuer: pdn_types::PdnId,
) -> Result<ScopedPeerGrant> {
    let mut found = None;
    let ok = eventually(|| async {
        Ok(receives
            .connections()
            .read_scoped_grants(receives_id, gives_id)
            .await?
            .into_iter()
            .any(|g| g.grant.issuer == issuer))
    })
    .await?;
    if ok {
        found = receives
            .connections()
            .read_scoped_grants(receives_id, gives_id)
            .await?
            .into_iter()
            .find(|g| g.grant.issuer == issuer);
    }
    found.ok_or_else(|| anyhow::anyhow!("scoped grant for {issuer} did not arrive"))
}

/// Allowed: X grants Y read on exactly one claim; Y's runtime receives the
/// capability and ticket over the pair, imports the namespace scoped, and
/// converges on exactly that entry — updates included.
///
/// Denied, outsider: a runtime with no connection to X and no ticket is
/// refused as unknown — before it ever holds anything of X's.
///
/// Denied, ticket without a grant: a runtime holding the grant's leaked
/// ticket but no grant of its own imports it and obtains nothing, probed
/// after the proven second wave plus several of its own reconcile
/// intervals.
///
/// Denied, existence hidden: X's other entries never reach Y, asserted
/// after a proven second replication wave (the sentinel update).
///
/// Denied, read-only cannot write: the grant's ticket carries no namespace
/// secret, so Y's local write into X's namespace is refused outright.
#[tokio::test(flavor = "multi_thread")]
async fn scoped_grant_flows_through_the_services() -> Result<()> {
    let rt_a = spawn_runtime().await?;
    let rt_b = spawn_runtime().await?;
    let rt_c = spawn_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let _z = rt_c.identity().create().await?;

    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;

    // X's data: the granted claim and two withheld ones.
    let email = EntryPath::new("contact/email")?;
    for (path, payload) in [
        ("contact/email", b"x@example.org".as_slice()),
        ("contact/phone", b"+1-555-0100".as_slice()),
        ("notes/diary", b"dear diary".as_slice()),
    ] {
        rt_a.data()
            .write(x, &EntryPath::new(path)?, payload)
            .await?;
    }

    // The scoped grant: read-only on exactly `contact/email`.
    rt_a.connections()
        .publish_scoped_grant(x, y, x, NonEmpty::new(claim_id_of(&x, &email)), false)
        .await?;

    // Y consumes it as the bootstrap cascade would: read the grant over
    // the pair, import the namespace scoped.
    let received = scoped_grant_patiently(&rt_b, y, x, x).await?;
    assert!(!received.grant.write);
    let leaked_ticket = received.ticket.clone();
    rt_b.data().import_scoped(x, received.ticket).await?;

    // Denied (outsider): before holding any ticket, the third runtime is
    // refused as specifically unknown — X was neither created nor imported
    // there, and no connection exists.
    let outsider_err = rt_c.data().read(x, &email).await.unwrap_err();
    assert!(
        outsider_err.downcast_ref::<UnknownIssuer>().is_some(),
        "an outsider must be refused as unknown, got: {outsider_err:?}"
    );

    // The ticket holder without a grant: the grant's ticket leaked to the
    // third runtime, which imports it scoped — a local registration that
    // starts its classified sync attempts. X's book resolves it to no
    // device and no grant, so every attempt is refused; the assertions
    // ride below, after the proven second wave.
    rt_c.data().import_scoped(x, leaked_ticket).await?;

    // Allowed: exactly the granted entry converges.
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(x, &email).await?.as_deref() == Some(&b"x@example.org"[..]))
        })
        .await?,
        "the granted entry did not reach the granted peer"
    );

    // Denied (read-only cannot write): no namespace secret rode the grant.
    assert!(
        rt_b.data().write(x, &email, b"overwrite").await.is_err(),
        "a write through a read-only scoped grant must be refused"
    );

    // Sentinel: an update to the granted claim proves a second replication
    // wave end to end, ordering the absence assertions below.
    rt_a.data().write(x, &email, b"x@new.example.org").await?;
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(x, &email).await?.as_deref() == Some(&b"x@new.example.org"[..]))
        })
        .await?,
        "the sentinel update did not reach the granted peer"
    );

    // Denied (existence hidden): after the proven second wave, Y's view of
    // X's namespace lists exactly the granted claim.
    let listed: Vec<String> = rt_b
        .data()
        .list(x, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["contact/email".to_owned()],
        "the granted peer's view must contain exactly the granted subset"
    );

    // Denied (ticket without a grant): the holder retried since its import
    // — the import's own sync attempt, a nudge per read/list, and one
    // re-dial per reconcile interval; waiting out three more intervals
    // after the proven second wave makes "it tried and was refused" what
    // keeps this green.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        rt_c.data().list(x, None).await?.is_empty(),
        "a leaked scoped ticket without a grant must deliver nothing"
    );
    assert!(rt_c.data().read(x, &email).await?.is_none());

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}
