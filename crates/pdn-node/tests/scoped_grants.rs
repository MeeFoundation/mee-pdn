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

use std::{cell::RefCell, time::Duration};

use anyhow::{ensure, Context, Result};
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, PeerGrant, Runtime,
    SpawnOptions, UnknownIssuer,
};
use pdn_types::EntryPath;
use test_utils::eventually;

mod common;
use common::establish_patiently;

const RECONCILE: Duration = Duration::from_millis(500);

async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// Poll until the peer's scoped grant for `issuer` is readable, handing
/// back the grant the poll itself observed: a second read is not the same
/// read.
async fn scoped_grant_patiently(
    receives: &Runtime,
    receives_id: pdn_types::PdnId,
    gives_id: pdn_types::PdnId,
    issuer: pdn_types::PdnId,
) -> Result<PeerGrant> {
    let observed = RefCell::new(None);
    let arrived = eventually(|| async {
        let found = receives
            .connections()
            .read_grants(receives_id, gives_id)
            .await?
            .into_iter()
            .find(|g| g.grant.issuer == issuer);
        let seen = found.is_some();
        *observed.borrow_mut() = found;
        Ok(seen)
    })
    .await?;
    ensure!(arrived, "scoped grant for {issuer} did not arrive");
    observed
        .into_inner()
        .context("the poll reported the grant and handed back nothing")
}

/// Allowed: X grants Y read on exactly one claim, and Y converges on
/// exactly that entry, updates included. Denied: an outsider with no
/// connection and no ticket is refused as unknown; a holder of the leaked
/// ticket without a grant obtains nothing; X's other entries never reach Y
/// (existence hidden); Y's read-only ticket carries no namespace secret, so
/// its local write is refused.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and every denied side in one place
async fn scoped_grant_flows_through_the_services() -> Result<()> {
    let rt_a = spawn_runtime().await?;
    let rt_b = spawn_runtime().await?;
    let rt_c = spawn_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let z = rt_c.identity().create().await?;

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
            .write(x, x, &EntryPath::new(path)?, payload)
            .await?;
    }

    // The scoped grant: read-only on exactly `contact/email`.
    rt_a.connections()
        .publish_grant(x, y, x, common::claims_on(x, &email, false))
        .await?;

    // Y reads the grant over the pair; the binder imports what it names. The
    // ticket is kept only to leak it below.
    let received = scoped_grant_patiently(&rt_b, y, x, x).await?;
    assert!(received.grant.claims.iter().all(|claim| !claim.write));
    let leaked_ticket = received.ticket;

    // Denied (outsider): refused as specifically unknown before holding any
    // ticket.
    let outsider_err = rt_c.data().read(z, x, &email).await.unwrap_err();
    assert!(
        outsider_err.downcast_ref::<UnknownIssuer>().is_some(),
        "an outsider must be refused as unknown, got: {outsider_err:?}"
    );

    // The leaked ticket, imported scoped on the third runtime: X's book
    // resolves it to no device and no grant. Asserted below, after the
    // proven second wave.
    rt_c.data().import_scoped(z, x, leaked_ticket).await?;

    // Allowed: exactly the granted entry converges.
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(y, x, &email).await?.as_deref() == Some(&b"x@example.org"[..]))
        })
        .await?,
        "the granted entry did not reach the granted peer"
    );

    // Denied (read-only cannot write): refused, and nothing acquired by it —
    // neither side's value moves.
    assert!(
        rt_b.data().write(y, x, &email, b"overwrite").await.is_err(),
        "a write through a read-only scoped grant must be refused"
    );
    assert_eq!(
        rt_b.data().read(y, x, &email).await?.as_deref(),
        Some(&b"x@example.org"[..]),
        "the refused write must not touch the grantee's own replica"
    );
    assert_eq!(
        rt_a.data().read(x, x, &email).await?.as_deref(),
        Some(&b"x@example.org"[..]),
        "the refused write must never reach the issuer"
    );

    // Sentinel: a proven second wave orders the absence assertions below.
    rt_a.data()
        .write(x, x, &email, b"x@new.example.org")
        .await?;
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(y, x, &email).await?.as_deref() == Some(&b"x@new.example.org"[..]))
        })
        .await?,
        "the sentinel update did not reach the granted peer"
    );

    // Denied (existence hidden).
    let listed: Vec<String> = rt_b
        .data()
        .list(y, x, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["contact/email".to_owned()],
        "the granted peer's view must contain exactly the granted subset"
    );

    // Denied (ticket without a grant): three more intervals after the proven
    // second wave make this "tried and refused".
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        rt_c.data().list(z, x, None).await?.is_empty(),
        "a leaked scoped ticket without a grant must deliver nothing"
    );
    assert!(rt_c.data().read(z, x, &email).await?.is_none());

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}
