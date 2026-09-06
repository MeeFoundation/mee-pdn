//! Sibling serving through the runtime services, ceremonies included: after
//! the issuer goes offline, a linked device — and one that joins only
//! afterwards — still catches up on the grant record over the pair and on
//! the claim from its sibling, with no import act anywhere: the grant
//! binder imports and forgets. Paired denial: an outsider holding a
//! sibling-minted ticket obtains nothing from the same serving device.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, ShareMode,
    SpawnOptions,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{eventually, TIMEOUT};

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

/// Poll until the peer's scoped grant for `issuer` is readable — the grant
/// record crossing, as distinct from the claim behind it.
async fn grant_arrives(
    receives: &Runtime,
    receives_id: PdnId,
    gives_id: PdnId,
    issuer: PdnId,
) -> Result<bool> {
    eventually(|| async {
        Ok(receives
            .connections()
            .read_grants(receives_id, gives_id)
            .await?
            .into_iter()
            .any(|g| g.grant.issuer == issuer))
    })
    .await
}

/// An unbound issuer counts as "not yet": until the binder acts on the
/// grant the issuer resolves to nothing at all.
async fn claim_arrives(
    reads: &Runtime,
    issuer: PdnId,
    path: &EntryPath,
    expected: &[u8],
) -> Result<bool> {
    eventually(|| async {
        Ok(matches!(
            reads.data().read(issuer, path).await,
            Ok(Some(payload)) if payload == expected
        ))
    })
    .await
}

/// Allowed: the laptop, linked after the fact and never introduced to Bob's
/// runtime, catches up on the pair, the grant, and the claim while Bob is
/// offline. Denied: Bob's withheld claim never reaches it — the phone
/// serves the claim set, not its holdings; Carol, holding a ticket the
/// phone itself minted, resolves in no audience directory and obtains
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_linked_device_catches_up_from_its_sibling_while_the_issuer_is_offline() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;
    let rt_carol = spawn_runtime().await?;

    // Alice lives on the phone; the laptop joins by the linking ceremony.
    let alice = rt_phone.identity().create().await?;
    let link_invite = rt_phone.identity().linking_invite(alice, None).await?;
    rt_laptop.identity().link(link_invite, TIMEOUT).await?;

    // Bob connects to Alice by establishment, writes a granted claim and a
    // withheld one, and publishes a scoped grant on the granted claim.
    let bob = rt_bob.identity().create().await?;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_phone, alice, &rt_bob, bob, invite).await?;
    let email = EntryPath::new("contact/email")?;
    let withheld = EntryPath::new("contact/phone")?;
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
    rt_bob.data().write(bob, &withheld, b"+1-555-0100").await?;
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, common::claims_on(bob, &email, false))
        .await?;

    // The binder imports what the grant names, unprompted.
    assert!(
        claim_arrives(&rt_phone, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach the phone while Bob was online"
    );

    // Bob goes offline before the laptop ever touches his grant.
    rt_bob.shutdown().await?;

    // Both crossed from the sibling, with the issuer away.
    assert!(
        grant_arrives(&rt_laptop, alice, bob, bob).await?,
        "the grant record did not reach the laptop from its sibling"
    );
    assert!(
        claim_arrives(&rt_laptop, bob, &email, b"bob@example.org").await?,
        "the granted claim did not catch up from the sibling with the issuer offline"
    );

    // Denied, existence hidden: the withheld claim is absent, and the
    // laptop's view lists exactly the granted subset.
    assert!(rt_laptop.data().read(bob, &withheld).await?.is_none());
    let listed: Vec<String> = rt_laptop
        .data()
        .list(bob, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["contact/email".to_owned()],
        "the laptop's view must contain exactly the granted subset"
    );

    // Denied, outsider: Carol's ticket is sibling-addressed and reachable,
    // and she resolves in no audience directory.
    let leaked = rt_phone.data().share(bob, ShareMode::Read).await?;
    rt_carol.data().import_scoped(bob, leaked).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        rt_carol.data().list(bob, None).await?.is_empty(),
        "a sibling-minted ticket without audience membership must deliver nothing"
    );
    assert!(rt_carol.data().read(bob, &email).await?.is_none());

    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_carol.shutdown().await?;
    Ok(())
}

/// A device that joins only after the issuer has gone offline still catches
/// up: every record crosses from the sibling, which is what the pair's
/// halves being pointed at the identity's own devices exists for — a
/// ticket names the devices of the side that minted it. Denied: Carol, as
/// above.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_the_issuer_left_catches_up_anyway() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;
    let rt_carol = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_phone, alice, &rt_bob, bob, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, common::claims_on(bob, &email, false))
        .await?;
    assert!(
        claim_arrives(&rt_phone, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach the phone while Bob was online"
    );

    // Bob leaves, and only then does the laptop join the identity.
    rt_bob.shutdown().await?;
    let link_invite = rt_phone.identity().linking_invite(alice, None).await?;
    rt_laptop.identity().link(link_invite, TIMEOUT).await?;

    assert!(
        eventually(|| async { Ok(rt_laptop.connections().list(alice).await?.contains(&bob)) })
            .await?,
        "the connection record did not reach the laptop from its sibling"
    );
    assert!(
        grant_arrives(&rt_laptop, alice, bob, bob).await?,
        "the grant record did not reach the laptop from its sibling"
    );
    assert!(
        claim_arrives(&rt_laptop, bob, &email, b"bob@example.org").await?,
        "the granted claim did not catch up from the sibling with the issuer offline"
    );

    // Denied, outsider.
    let leaked = rt_phone.data().share(bob, ShareMode::Read).await?;
    rt_carol.data().import_scoped(bob, leaked).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        rt_carol.data().list(bob, None).await?.is_empty(),
        "a sibling-minted ticket without audience membership must deliver nothing"
    );
    assert!(rt_carol.data().read(bob, &email).await?.is_none());

    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_carol.shutdown().await?;
    Ok(())
}

/// The binder's other direction: a withdrawn grant takes the namespace back
/// out, and the issuer becomes unknown again rather than resolving to a
/// stale replica.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_grant_takes_the_namespace_back_out() -> Result<()> {
    let rt_alice = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_alice.identity().create().await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_alice, alice, &rt_bob, bob, invite).await?;

    let email = EntryPath::new("contact/email")?;
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, common::claims_on(bob, &email, false))
        .await?;
    assert!(
        claim_arrives(&rt_alice, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach Alice"
    );

    // Withdrawal: the tombstone replicates, the binder drops what it
    // imported, and the issuer resolves to nothing again.
    rt_bob.connections().withdraw_grant(bob, alice, bob).await?;
    assert!(
        eventually(|| async { Ok(rt_alice.data().read(bob, &email).await.is_err()) }).await?,
        "the withdrawn namespace was still bound on Alice"
    );

    rt_alice.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}
