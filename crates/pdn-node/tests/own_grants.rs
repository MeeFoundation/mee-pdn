//! What a hosted identity has published toward a peer, read from its own
//! half of the connection's metadata pair: the capability the issuer reads
//! back, a republication and a withdrawal seen through it, and a sibling
//! device reading a grant it did not publish.
//!
//! Every positive read here carries its denial in the same place
//! (`code-practices/access-control-tests.md`), and for this operation the
//! tightest party is a second identity hosted on the same runtime: it has
//! a directory of its own, and the read resolves the pair through the
//! acting identity's directory. The denial is probed in two degrees. A
//! co-hosted identity with no pair toward that peer catches a lookup keyed
//! on the peer instead of the directory: publishing opens and caches the
//! pair, so such a lookup hands that identity the other's record. A
//! co-hosted identity holding its own pair toward the same peer catches
//! the narrower slip the first cannot — a lookup keyed on the peer among
//! the identities that do hold one. Beside both stands the positive read,
//! without which a denial whose expected answer is nothing is satisfied by
//! an implementation that answers nothing to everyone. A party on another
//! node is no control at all: nothing lets it address the pair, so no call
//! can be made.
//!
//! The read hands back the capability alone. No assertion here says the
//! ticket is absent, because the read's type carries none — an absence has
//! no test that could fail.

use anyhow::Result;
use pdn_node::{ConnectionsService as _, IdentityService as _, UnknownIdentity};
use pdn_types::EntryPath;
use test_utils::eventually;

mod common;
use common::{claims_on, establish_patiently, link_patiently, memory_runtime};

/// The issuer reads what it published — the granted issuer, the audience,
/// the exact claim set and the write right of each claim — beside both
/// degrees of the denial and the refusal of an identity this runtime does
/// not host.
#[tokio::test(flavor = "multi_thread")]
async fn an_issuer_reads_what_it_published_and_a_co_hosted_identity_reads_none_of_it() -> Result<()>
{
    let a = memory_runtime().await?;
    let peer = memory_runtime().await?;

    // X and Y are hosted side by side; only X connects to P for now.
    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    let p = peer.identity().create().await?;
    let invite = a.connections().invite(x, None).await?;
    establish_patiently(&peer, p, &a, x, invite).await?;

    // Two claims, one of them writable, so the read is asserted on the
    // exact set rather than on its existence.
    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    let mut claims = claims_on(x, &email, false);
    claims.push(claims_on(x, &phone, true).head);
    a.connections()
        .publish_grant(x, p, x, claims.clone())
        .await?;

    let own = a.connections().read_own_grants(x, p).await?;
    assert_eq!(own.len(), 1, "one record per granted issuer: {own:?}");
    let grant = own.first().expect("one grant");
    assert_eq!(grant.issuer, x);
    assert_eq!(grant.audience, p);
    assert_eq!(grant.claims, claims);

    // Denied, first degree: Y is hosted beside X and holds no pair toward
    // P, so its own directory answers nothing — while X's record, read on
    // the same runtime a line above, is what makes the emptiness a denial
    // rather than an operation that answers nothing to everyone.
    assert!(
        a.connections().read_own_grants(y, p).await?.is_empty(),
        "an identity with no connection to the peer must read no grant of another identity's"
    );

    // Denied, second degree and the tighter one: Y establishes its own
    // connection to the same peer and publishes a grant of its own data.
    // Both identities now hold a pair toward P, so a lookup keyed on the
    // peer would hand one of them the other's record.
    let invite = a.connections().invite(y, None).await?;
    establish_patiently(&peer, p, &a, y, invite).await?;
    let y_claims = claims_on(y, &email, false);
    a.connections()
        .publish_grant(y, p, y, y_claims.clone())
        .await?;

    let y_own = a.connections().read_own_grants(y, p).await?;
    assert_eq!(y_own.len(), 1, "one record per granted issuer: {y_own:?}");
    let y_grant = y_own.first().expect("one grant");
    assert_eq!(y_grant.issuer, y, "Y must read its own grant, never X's");
    assert_eq!(y_grant.claims, y_claims);

    let x_own = a.connections().read_own_grants(x, p).await?;
    assert_eq!(x_own.len(), 1, "one record per granted issuer: {x_own:?}");
    assert_eq!(x_own.first().expect("one grant").claims, claims);

    // An identity this runtime neither created nor linked is refused, as
    // every other connections operation refuses it.
    let elsewhere = peer.identity().create().await?;
    let err = a
        .connections()
        .read_own_grants(elsewhere, p)
        .await
        .expect_err("an unhosted identity must be refused");
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());

    a.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}

/// A republication reports the second claim set, and a withdrawal reports
/// no grant for that issuer. The republication is what discriminates here:
/// after the withdrawal the read is empty, which is also what a broken
/// read would answer, so the assertion that carries the weight is the one
/// before it.
#[tokio::test(flavor = "multi_thread")]
async fn a_republication_and_a_withdrawal_are_visible_to_the_issuer() -> Result<()> {
    let a = memory_runtime().await?;
    let peer = memory_runtime().await?;

    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    let p = peer.identity().create().await?;
    let invite = a.connections().invite(x, None).await?;
    establish_patiently(&peer, p, &a, x, invite).await?;

    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    a.connections()
        .publish_grant(x, p, x, claims_on(x, &email, false))
        .await?;
    let republished = claims_on(x, &phone, true);
    a.connections()
        .publish_grant(x, p, x, republished.clone())
        .await?;

    let own = a.connections().read_own_grants(x, p).await?;
    assert_eq!(own.len(), 1, "a republication replaces, never accretes");
    assert_eq!(own.first().expect("one grant").claims, republished);

    // Denied in the same place: the co-hosted identity reads nothing of
    // this while the republished set is readable to its issuer.
    assert!(
        a.connections().read_own_grants(y, p).await?.is_empty(),
        "an identity beside the issuer must read no grant of the issuer's"
    );

    a.connections().withdraw_grant(x, p, x).await?;
    assert!(
        a.connections().read_own_grants(x, p).await?.is_empty(),
        "a withdrawn grant must not be readable to the issuer that withdrew it"
    );

    a.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}

/// A sibling device reads a grant it did not publish, once the record and
/// its payload have replicated to it over the pair.
///
/// The emptiness before that is the contract and not an assertion here:
/// nothing holds replication back, so a scenario that asserted it would be
/// racing its own subject. The waiting half is what this stages; the other
/// half is stated in the service's docs and in `pdn-node/core.md`, guarded
/// by this suite's place in the change's stress pass rather than by a pin
/// this code offers no place for (`code-practices/flaky-tests.md`).
#[tokio::test(flavor = "multi_thread")]
async fn a_sibling_device_reads_a_grant_it_did_not_publish() -> Result<()> {
    let phone = memory_runtime().await?;
    let laptop = memory_runtime().await?;
    let peer = memory_runtime().await?;

    // The laptop links first, so everything about the connection and the
    // grant below reaches it by replication alone.
    let alice = phone.identity().create().await?;
    link_patiently(&laptop, &phone, alice).await?;
    let bob = peer.identity().create().await?;
    let invite = phone.connections().invite(alice, None).await?;
    establish_patiently(&peer, bob, &phone, alice, invite).await?;

    let email = EntryPath::new("contact/email")?;
    let claims = claims_on(alice, &email, true);
    phone
        .connections()
        .publish_grant(alice, bob, alice, claims.clone())
        .await?;

    assert!(
        eventually(|| async {
            Ok(laptop
                .connections()
                .read_own_grants(alice, bob)
                .await?
                .iter()
                .any(|grant| {
                    grant.issuer == alice && grant.audience == bob && grant.claims == claims
                }))
        })
        .await?,
        "the grant published on the phone never became readable on the sibling"
    );

    // Denied in the same place, on the device that just read: an identity
    // hosted beside Alice there holds no pair toward Bob and reads nothing.
    let other = laptop.identity().create().await?;
    assert!(
        laptop
            .connections()
            .read_own_grants(other, bob)
            .await?
            .is_empty(),
        "an identity beside the sibling's must read no grant of Alice's"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}
