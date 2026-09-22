//! The write side of mixed per-claim rights (the read denials are in
//! `scoped_grants.rs`): Bob grants Alice `contact/email` read-only and
//! `contact/phone` read-write. Denied at the surface: a write at the
//! read-only claim is refused at the call site. Denied at the gate: a
//! forced write past the courtesy (`write_unguarded`, hence `test-util`
//! only) never reaches Bob and is retracted, surfaced as an event.
#![cfg(feature = "test-util")]

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, GrantedClaim, IdentityService as _, Runtime,
    SpawnOptions, WriteNotGranted,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{eventually, TIMEOUT};

mod common;
use common::establish_patiently;

/// The forced write draws its rejection and retraction in a sub-second
/// session.
const RECONCILE: Duration = Duration::from_millis(300);

async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// Poll until `reads` sees `expected` at `path` under `issuer`.
async fn reads_value(
    reads: &Runtime,
    acting: PdnId,
    issuer: PdnId,
    path: &EntryPath,
    expected: &[u8],
) -> Result<bool> {
    eventually(|| async {
        Ok(matches!(
            reads.data().read(acting, issuer, path).await,
            Ok(Some(payload)) if payload == expected
        ))
    })
    .await
}

/// Allowed: Alice reads both claims and her overwrite of the read-write
/// claim reaches Bob. Denied at the surface: her write at the read-only
/// claim is refused at the call site, before the replica is touched. Denied
/// at the gate: a forced write past the courtesy never reaches Bob, and
/// Alice's provisional entry is retracted back to Bob's value, surfaced as
/// an event.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and both denied sides in one place
async fn mixed_grant_email_read_only_phone_read_write() -> Result<()> {
    let rt_bob = spawn_runtime().await?;
    let rt_alice = spawn_runtime().await?;
    let bob = rt_bob.identity().create().await?;
    let alice = rt_alice.identity().create().await?;

    // Alice subscribes to her retraction events before anything can fire.
    let mut retractions = rt_alice.subscribe_retractions().await;

    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_alice, alice, &rt_bob, bob, invite).await?;

    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    rt_bob
        .data()
        .write(bob, bob, &email, b"bob@example.org")
        .await?;
    rt_bob
        .data()
        .write(bob, bob, &phone, b"+1-555-0100")
        .await?;

    // The mixed grant: email read-only, phone read-write — one publish.
    let mut claims = common::claims_on(bob, &email, false);
    claims.push(GrantedClaim {
        claim: pdn_node::claim_id_of(&bob, &phone),
        write: true,
    });
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, claims)
        .await?;

    // Allowed: the binder imports the namespace by itself, and both claims
    // become readable at Alice.
    assert!(
        reads_value(&rt_alice, alice, bob, &email, b"bob@example.org").await?,
        "the read-only claim did not reach Alice"
    );
    assert!(
        reads_value(&rt_alice, alice, bob, &phone, b"+1-555-0100").await?,
        "the read-write claim did not reach Alice"
    );

    // Allowed: Alice overwrites the read-write claim, and Bob converges.
    rt_alice
        .data()
        .write(alice, bob, &phone, b"+7-999-0001")
        .await?;
    assert!(
        reads_value(&rt_bob, bob, bob, &phone, b"+7-999-0001").await?,
        "the write-granted claim did not round-trip to the issuer"
    );

    // Denied at the surface: a write at the read-only claim is refused at
    // the call site, and Bob's value is untouched.
    let refused = rt_alice
        .data()
        .write(alice, bob, &email, b"alice-overwrite")
        .await
        .expect_err("a write at a read-only claim must be refused");
    assert!(
        refused.downcast_ref::<WriteNotGranted>().is_some(),
        "the courtesy refusal must be WriteNotGranted, got: {refused:?}"
    );
    // Nothing was written locally — Alice still reads Bob's value.
    assert_eq!(
        rt_alice.data().read(alice, bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the refused write must not touch the local replica"
    );

    // Denied at the gate, with recovery: the secret rode the write ticket, so
    // the forced entry is signed and reconciles; Bob's gate refuses it in
    // band, and Alice's node retracts on the first rejection.
    rt_alice
        .data()
        .write_unguarded(alice, bob, &email, b"forced-by-alice")
        .await?;
    // It was stored locally (provisional) before the verdict.
    assert_eq!(
        rt_alice.data().read(alice, bob, &email).await?.as_deref(),
        Some(&b"forced-by-alice"[..]),
        "the forced write must be stored locally before the verdict"
    );

    // The event carries the provenance a host recovers from: the lost
    // payload's address in the blob store, and the device that decided.
    let event = tokio::time::timeout(test_utils::TIMEOUT, retractions.recv())
        .await
        .map_err(|_elapsed| anyhow::anyhow!("no retraction event within the timeout"))??;
    assert_eq!(event.issuer, bob);
    assert_eq!(event.path, email);
    assert_eq!(
        event.content_hash,
        *blake3::hash(b"forced-by-alice").as_bytes(),
        "the event must address the payload that was lost"
    );
    assert_eq!(
        event.decided_by,
        rt_alice.node_id(),
        "the deciding device is the one that received the rejection"
    );

    // Local view returns to Bob's kept value, and Bob never took the forced
    // one.
    assert!(
        reads_value(&rt_alice, alice, bob, &email, b"bob@example.org").await?,
        "the retracted entry did not return to the issuer's value locally"
    );
    assert_eq!(
        rt_bob.data().read(bob, bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the forced write must never reach the issuer"
    );

    rt_bob.shutdown().await?;
    rt_alice.shutdown().await?;
    Ok(())
}

/// A claim leaving the grant does not destroy what the issuer accepted
/// under it: Bob narrows Alice's grant from email and phone to phone alone,
/// and his copy of her email entry stays. Alice goes on offering the entry
/// every session, since Bob's narrowed egress no longer serves it back; his
/// gate refuses it silently, because a rejection is what makes Alice
/// destroy her copy.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, before and after the narrowing in one place
async fn narrowing_a_grant_keeps_what_the_issuer_already_accepted() -> Result<()> {
    let rt_bob = spawn_runtime().await?;
    let rt_alice = spawn_runtime().await?;
    let bob = rt_bob.identity().create().await?;
    let alice = rt_alice.identity().create().await?;

    let mut retractions = rt_alice.subscribe_retractions().await;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_alice, alice, &rt_bob, bob, invite).await?;

    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    rt_bob
        .data()
        .write(bob, bob, &email, b"bob@example.org")
        .await?;
    rt_bob
        .data()
        .write(bob, bob, &phone, b"+1-555-0100")
        .await?;

    // Both claims carry write, and Alice's write at email is accepted.
    let mut both = common::claims_on(bob, &email, true);
    both.push(GrantedClaim {
        claim: pdn_node::claim_id_of(&bob, &phone),
        write: true,
    });
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, both)
        .await?;
    assert!(
        reads_value(&rt_alice, alice, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach Alice"
    );
    rt_alice
        .data()
        .write(alice, bob, &email, b"alice@example.org")
        .await?;
    assert!(
        reads_value(&rt_bob, bob, bob, &email, b"alice@example.org").await?,
        "the granted write did not reach the issuer"
    );

    // Narrowed to phone alone: nothing is withdrawn, but email leaves
    // Alice's read slice.
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, common::claims_on(bob, &phone, true))
        .await?;
    let email_claim = pdn_node::claim_id_of(&bob, &email);
    assert!(
        eventually(|| async {
            let grants = rt_alice.connections().read_grants(alice, bob).await?;
            Ok(grants.iter().all(|peer_grant| {
                !peer_grant
                    .grant
                    .claims
                    .iter()
                    .any(|granted| granted.claim == email_claim)
            }))
        })
        .await?,
        "the narrowed grant did not cross the connection"
    );

    // A write at the claim that kept its write round-trips after the
    // narrowing, so sessions have run on the replica holding the email entry.
    rt_alice
        .data()
        .write(alice, bob, &phone, b"+7-999-0001")
        .await?;
    assert!(
        reads_value(&rt_bob, bob, bob, &phone, b"+7-999-0001").await?,
        "the still-granted write did not round-trip after the narrowing"
    );

    // Neither side lost the accepted entry, and nothing was retracted.
    assert_eq!(
        rt_alice.data().read(alice, bob, &email).await?.as_deref(),
        Some(&b"alice@example.org"[..]),
        "the audience lost an entry the issuer accepted and still holds"
    );
    assert_eq!(
        rt_bob.data().read(bob, bob, &email).await?.as_deref(),
        Some(&b"alice@example.org"[..]),
        "the issuer lost the entry it accepted"
    );
    assert!(
        retractions.try_recv().is_err(),
        "narrowing a grant retracted an already accepted entry"
    );

    rt_bob.shutdown().await?;
    rt_alice.shutdown().await?;
    Ok(())
}

/// A retracted provisional write does not flap back from a sibling device
/// that replicated it: the marker crosses to the laptop and it drops the
/// entry too.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // two devices, allowed and the flap denial in one place
async fn a_retraction_does_not_flap_back_from_a_sibling() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    // Alice lives on the phone; the laptop joins by linking.
    let alice = rt_phone.identity().create().await?;
    let link_invite = rt_phone.identity().linking_invite(alice, None).await?;
    rt_laptop.identity().link(link_invite, TIMEOUT).await?;

    // Mixed, so the grant ships a write ticket while email stays outside the
    // write set.
    let bob = rt_bob.identity().create().await?;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_phone, alice, &rt_bob, bob, invite).await?;
    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    rt_bob
        .data()
        .write(bob, bob, &email, b"bob@example.org")
        .await?;
    let mut claims = common::claims_on(bob, &email, false);
    claims.push(GrantedClaim {
        claim: pdn_node::claim_id_of(&bob, &phone),
        write: true,
    });
    rt_bob
        .connections()
        .publish_grant(bob, alice, bob, claims)
        .await?;

    // Both devices converge on Bob's value through their binders.
    assert!(
        reads_value(&rt_phone, alice, bob, &email, b"bob@example.org").await?,
        "the phone did not converge on the granted claim"
    );
    assert!(
        reads_value(&rt_laptop, alice, bob, &email, b"bob@example.org").await?,
        "the laptop did not converge on the granted claim"
    );

    // The phone forces the read-only claim and may replicate it to the laptop
    // before Bob's gate refuses it. Whichever path runs, the forged value
    // survives on neither device.
    rt_phone
        .data()
        .write_unguarded(alice, bob, &email, b"forced-by-phone")
        .await?;
    assert!(
        reads_value(&rt_phone, alice, bob, &email, b"bob@example.org").await?,
        "the phone did not retract its forced write"
    );
    assert!(
        reads_value(&rt_laptop, alice, bob, &email, b"bob@example.org").await?,
        "the laptop did not converge on Bob's value — the forged entry survived on the sibling"
    );
    // Bob never took the forced value.
    assert_eq!(
        rt_bob.data().read(bob, bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the forced write must never reach the issuer"
    );

    rt_bob.shutdown().await?;
    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    Ok(())
}

/// A retraction verdict is recorded in the directory of the identity
/// whose author it names and in no other: one identity of a node writes
/// past the grant it holds, and the co-located identity granted the same
/// claim keeps its directory and its replica as they were.
///
/// Denied: the co-located identity's directory gains no marker and its
/// replica keeps the issuer's value, asserted after the writing
/// identity's own marker has demonstrably landed.
#[tokio::test(flavor = "multi_thread")]
async fn a_retraction_marks_only_the_directory_of_the_author_it_names() -> Result<()> {
    let rt_bob = spawn_runtime().await?;
    let rt_alice = spawn_runtime().await?;
    let bob = rt_bob.identity().create().await?;
    let work = rt_alice.identity().create().await?;
    let leisure = rt_alice.identity().create().await?;

    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    rt_bob
        .data()
        .write(bob, bob, &email, b"bob@example.org")
        .await?;
    rt_bob
        .data()
        .write(bob, bob, &phone, b"+1-555-0100")
        .await?;

    // Both identities of Alice's node hold the same mixed grant, so both
    // hold a replica of Bob's namespace, both carry the namespace secret
    // the write claim ships, and either could carry a marker.
    for identity in [work, leisure] {
        let invite = rt_bob.connections().invite(bob, None).await?;
        establish_patiently(&rt_alice, identity, &rt_bob, bob, invite).await?;
        let mut claims = common::claims_on(bob, &email, false);
        claims.push(GrantedClaim {
            claim: pdn_node::claim_id_of(&bob, &phone),
            write: true,
        });
        rt_bob
            .connections()
            .publish_grant(bob, identity, bob, claims)
            .await?;
        assert!(
            reads_value(&rt_alice, identity, bob, &email, b"bob@example.org").await?,
            "the granted claim did not reach {identity} — the premise of this test"
        );
    }

    // A granted write of that identity, round-tripped to Bob first: a
    // forced write is only judged where the writer's own egress is open
    // and its writes are admitted, and this is what proves both.
    rt_alice
        .data()
        .write(work, bob, &phone, b"+7-999-0001")
        .await?;
    assert!(
        reads_value(&rt_bob, bob, bob, &phone, b"+7-999-0001").await?,
        "the write-granted claim did not round-trip — the premise of this test"
    );

    // One identity writes past the read-only grant; Bob's gate refuses it.
    rt_alice
        .data()
        .write_unguarded(work, bob, &email, b"forced-by-work")
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_alice.sync().retraction_markers(work).await?.is_empty()) })
            .await?,
        "the verdict never reached the writing identity's own directory"
    );
    assert!(
        reads_value(&rt_alice, work, bob, &email, b"bob@example.org").await?,
        "the retraction did not take the forced write back to the issuer's value"
    );

    // Denied: nothing of it in the co-located identity, directory or
    // replica.
    assert!(
        rt_alice
            .sync()
            .retraction_markers(leisure)
            .await?
            .is_empty(),
        "a verdict was recorded in a co-located identity's directory"
    );
    assert_eq!(
        rt_alice.data().read(leisure, bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "a co-located identity's replica was touched by another identity's retraction"
    );

    rt_alice.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}
