//! Sibling serving through the runtime services, ceremonies included: an
//! identity is created on one runtime and linked onto a second, the
//! connection and the scoped grant arrive by establishment and the pair,
//! and after the issuer goes offline the linked device still catches up —
//! the grant record over the device-replicated pair, the claim itself from
//! its sibling device, served per the locally replicated grant. No import
//! act appears in either scenario: the grant binder is what turns a grant
//! that replicated in into an imported namespace, and a withdrawal back
//! into a forgotten one. Paired denial per
//! `code-practices/access-control-tests.md`: an outsider holding a
//! sibling-minted ticket — the same serving device demonstrably answers the
//! sibling — obtains nothing.

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    claim_id_of, ConnectionsService as _, DataService as _, IdentityService as _, NonEmpty,
    Runtime, ShareMode, SpawnOptions,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{eventually, TIMEOUT};

mod common;
use common::establish_patiently;

/// The reconcile cadence of these scenarios — the outsider denial below is
/// "it retried over several intervals and was refused", made cheap by
/// injecting a sub-second interval.
const RECONCILE: Duration = Duration::from_millis(500);

/// Spawn a runtime with the tests' short reconcile cadence.
async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn_with(SpawnOptions {
        reconcile_interval: RECONCILE,
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

/// Poll until `reads` sees `expected` at `path` in `issuer`'s namespace.
/// An unbound issuer counts as "not yet", not as a failure: nothing imports
/// the namespace up front any more, so until the binder acts on the grant
/// the issuer resolves to nothing at all.
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

/// Allowed: Alice's laptop — linked after the fact, never introduced to
/// Bob's runtime directly — catches up on the pair, the grant, and the
/// granted claim while Bob is offline, with no import act anywhere: the
/// records cross the device-replicated stores, the binder imports what the
/// grant names, and the claim is served by the phone per the replicated
/// grant.
///
/// Denied, existence hidden: Bob's withheld claim never reaches the
/// laptop — the phone serves the claim set, not its holdings.
///
/// Denied, outsider with a sibling ticket: a runtime holding a ticket the
/// phone itself minted resolves in no audience directory and obtains
/// nothing, probed after the laptop's proven convergence plus several of
/// its own reconcile intervals.
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
        .publish_grant(
            bob,
            alice,
            bob,
            NonEmpty::new(claim_id_of(&bob, &email)),
            false,
        )
        .await?;

    // The phone converges on the granted claim while Bob is online — the
    // binder imports what the grant names, unprompted.
    assert!(
        claim_arrives(&rt_phone, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach the phone while Bob was online"
    );

    // Bob goes offline before the laptop ever touches his grant.
    rt_bob.shutdown().await?;

    // The grant record crossed device-to-device, and the claim behind it
    // followed — from the sibling, with the issuer away and no import act.
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

    // Denied, outsider: Carol holds a ticket the phone itself minted —
    // reachable, sibling-addressed — but resolves in no audience
    // directory. After the laptop's proven convergence and several of her
    // own reconcile intervals, she holds nothing.
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
/// out. What the binder imported it also forgets once the record it stood on
/// is gone, so the grantee stops holding bytes no grant justifies — and the
/// issuer becomes unknown again rather than resolving to a stale replica.
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
        .publish_grant(
            bob,
            alice,
            bob,
            NonEmpty::new(claim_id_of(&bob, &email)),
            false,
        )
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
