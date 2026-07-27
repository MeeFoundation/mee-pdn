//! Mixed per-claim rights end to end through the runtime services — the
//! shape this change exists for: over one connection Bob grants Alice
//! `contact/email` read-only and `contact/phone` read-write.
//!
//! Allowed: Alice reads both claims, and her overwrite of the read-write
//! claim reaches Bob. Denied at the surface: her write at the read-only
//! claim is refused at the call site ([`WriteNotGranted`]), before the
//! replica is touched. Denied at the gate, with recovery: a forced write
//! outside the write set — the courtesy bypassed — never reaches Bob, and
//! Alice's provisional entry is retracted back to Bob's value, surfaced as
//! an event. The read denials stay with `scoped_grants.rs`; this file is
//! the write side, paired per `code-practices/access-control-tests.md`.
//!
//! The forced write goes through `RuntimeDataService::write_unguarded`, so
//! this file compiles only under the `test-util` feature — the `just` dev
//! recipes enable it; a bare `cargo build`/`check` omits the file.
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

/// A brisk reconcile cadence, so the forced write draws its in-band rejection
/// — and its retraction — in a sub-second session rather than the production
/// reconcile interval.
const RECONCILE: Duration = Duration::from_millis(300);

async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn_with(SpawnOptions {
        reconcile_interval: RECONCILE,
    })
    .await
}

/// Poll until `reads` sees `expected` at `path` under `issuer`.
async fn reads_value(
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
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
    rt_bob.data().write(bob, &phone, b"+1-555-0100").await?;

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
        reads_value(&rt_alice, bob, &email, b"bob@example.org").await?,
        "the read-only claim did not reach Alice"
    );
    assert!(
        reads_value(&rt_alice, bob, &phone, b"+1-555-0100").await?,
        "the read-write claim did not reach Alice"
    );

    // Allowed: Alice overwrites the read-write claim, and Bob converges.
    rt_alice.data().write(bob, &phone, b"+7-999-0001").await?;
    assert!(
        reads_value(&rt_bob, bob, &phone, b"+7-999-0001").await?,
        "the write-granted claim did not round-trip to the issuer"
    );

    // Denied at the surface: a write at the read-only claim is refused at
    // the call site, and Bob's value is untouched.
    let refused = rt_alice
        .data()
        .write(bob, &email, b"alice-overwrite")
        .await
        .expect_err("a write at a read-only claim must be refused");
    assert!(
        refused.downcast_ref::<WriteNotGranted>().is_some(),
        "the courtesy refusal must be WriteNotGranted, got: {refused:?}"
    );
    // Nothing was written locally — Alice still reads Bob's value.
    assert_eq!(
        rt_alice.data().read(bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the refused write must not touch the local replica"
    );

    // Denied at the gate, with recovery: a client that bypasses the
    // courtesy writes the read-only claim anyway — the secret rode the
    // write ticket, so the entry is signed and reconciles, but Bob's gate
    // refuses it every session, and says so in band. Alice's node retracts
    // it on the first rejection and emits the event; her view returns to
    // Bob's value.
    rt_alice
        .data()
        .write_unguarded(bob, &email, b"forced-by-alice")
        .await?;
    // It was stored locally (provisional) before the verdict.
    assert_eq!(
        rt_alice.data().read(bob, &email).await?.as_deref(),
        Some(&b"forced-by-alice"[..]),
        "the forced write must be stored locally before the verdict"
    );

    // The retraction event names the forced entry — including the provenance
    // the host is meant to recover from: the address of the lost payload in
    // the blob store, and the device that decided.
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
        reads_value(&rt_alice, bob, &email, b"bob@example.org").await?,
        "the retracted entry did not return to the issuer's value locally"
    );
    assert_eq!(
        rt_bob.data().read(bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the forced write must never reach the issuer"
    );

    rt_bob.shutdown().await?;
    rt_alice.shutdown().await?;
    Ok(())
}

/// The narrowing half of the gate: a claim leaving the grant does not destroy
/// what the issuer accepted under it while it was there. Bob grants Alice
/// write on `contact/email`, keeps her entry, then republishes the grant on
/// `contact/phone` alone — the grant narrows, it is not withdrawn.
///
/// Alice goes on offering the entry every session, because Bob's narrowed
/// egress no longer serves it back and the two sets read as divergent. Bob's
/// gate refuses it, and refuses it *silently*: he holds that entry, and a
/// rejection is what makes Alice destroy her copy of it.
#[tokio::test(flavor = "multi_thread")]
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
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
    rt_bob.data().write(bob, &phone, b"+1-555-0100").await?;

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
        reads_value(&rt_alice, bob, &email, b"bob@example.org").await?,
        "the granted claim did not reach Alice"
    );
    rt_alice
        .data()
        .write(bob, &email, b"alice@example.org")
        .await?;
    assert!(
        reads_value(&rt_bob, bob, &email, b"alice@example.org").await?,
        "the granted write did not reach the issuer"
    );

    // Bob narrows the grant to phone alone. Nothing is withdrawn, so the
    // namespace stays bound at Alice — but email leaves her read slice, so
    // Bob stops serving that entry back and the two sets read as divergent.
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
    rt_alice.data().write(bob, &phone, b"+7-999-0001").await?;
    assert!(
        reads_value(&rt_bob, bob, &phone, b"+7-999-0001").await?,
        "the still-granted write did not round-trip after the narrowing"
    );

    // Neither side lost the accepted entry, and nothing was retracted.
    assert_eq!(
        rt_alice.data().read(bob, &email).await?.as_deref(),
        Some(&b"alice@example.org"[..]),
        "the audience lost an entry the issuer accepted and still holds"
    );
    assert_eq!(
        rt_bob.data().read(bob, &email).await?.as_deref(),
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

/// The sibling half of retraction: a retracted provisional write does not
/// flap back from a sibling device that replicated it. Alice hosts two
/// devices; her phone forces a write Bob refuses, the laptop replicates
/// that provisional entry from its sibling, and once the phone retracts,
/// the marker crosses to the laptop and the laptop drops it too — so a
/// reader on the laptop returns to Bob's value rather than the forged one.
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

    // The grant is mixed — email read-only, phone read-write — so it ships
    // a write ticket (the secret) while email stays outside the write set.
    // The phone can thus produce an email entry the gate refuses.
    let bob = rt_bob.identity().create().await?;
    let invite = rt_bob.connections().invite(bob, None).await?;
    establish_patiently(&rt_phone, alice, &rt_bob, bob, invite).await?;
    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;
    rt_bob.data().write(bob, &email, b"bob@example.org").await?;
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
        reads_value(&rt_phone, bob, &email, b"bob@example.org").await?,
        "the phone did not converge on the granted claim"
    );
    assert!(
        reads_value(&rt_laptop, bob, &email, b"bob@example.org").await?,
        "the laptop did not converge on the granted claim"
    );

    // The phone forces the read-only claim under its own author. It stores
    // locally and, as a sibling, may replicate the provisional entry to the
    // laptop before Bob's gate refuses it. Bob nacks the phone in-band, so the
    // phone retracts deterministically and records the marker; the marker
    // replicates to the laptop, which drops the forged entry too (and the
    // laptop, reaching Bob's gate itself, would be refused there as well).
    // Whichever path runs, the forged value survives on neither device — it
    // does not flap back from the sibling.
    rt_phone
        .data()
        .write_unguarded(bob, &email, b"forced-by-phone")
        .await?;
    assert!(
        reads_value(&rt_phone, bob, &email, b"bob@example.org").await?,
        "the phone did not retract its forced write"
    );
    assert!(
        reads_value(&rt_laptop, bob, &email, b"bob@example.org").await?,
        "the laptop did not converge on Bob's value — the forged entry survived on the sibling"
    );
    // Bob never took the forced value.
    assert_eq!(
        rt_bob.data().read(bob, &email).await?.as_deref(),
        Some(&b"bob@example.org"[..]),
        "the forced write must never reach the issuer"
    );

    rt_bob.shutdown().await?;
    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    Ok(())
}
