//! A granted replica reaches every device of its issuer, not only the one
//! that published the grant: the grant sweep derives the replica's whole
//! contact set from the device records on both sides — the issuer's
//! published devices, the audience identities' siblings — plus the grant
//! ticket's addressing, re-derived per sweep. The scenarios turn the
//! publishing device off and require convergence from another; the
//! contact-set observation rides the `test-util` surface, so entering and
//! leaving the set is asserted rather than slept on. Paired denial per
//! `code-practices/access-control-tests.md`: the serving sibling gives a
//! bare ticket holder nothing, probed right after it demonstrably served
//! the granted audience.
//!
//! The grant sweep's replica lifecycle is asserted on the same surface:
//! the replica shared by co-hosted audiences (ADR-0009) leaves only with
//! the last withdrawn grant, the unbind decision counts durable grant
//! records rather than the binders' bookkeeping, and a replica forgotten
//! out from under the bookkeeping re-imports on the pair's next sweep.
//!
//! The contact observation goes through `RuntimeDataService::contacts_of`,
//! so this file compiles only under the `test-util` feature — the `just`
//! dev recipes enable it; a bare `cargo build`/`check` omits the file.
#![cfg(feature = "test-util")]

use std::{cell::RefCell, time::Duration};

use anyhow::{ensure, Context, Result};
use data_layer::{own_ticket_kind, ConnectionMetadataStore, PrivateMetadataStore, SyncNode};
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, ShareMode,
    SpawnOptions, UnknownIssuer,
};
use pdn_types::{EntryPath, NodeId, PdnId};
use test_utils::eventually;

mod common;
use common::{establish_patiently, granted_patiently, link_patiently, link_probe};

/// The reconcile cadence of these scenarios — the denials below are "it
/// retried over several intervals and was refused", made cheap by
/// injecting a sub-second interval.
const RECONCILE: Duration = Duration::from_millis(500);

/// Spawn a runtime with the tests' short reconcile cadence.
async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// Whether `device` is among the tracked contacts of `issuer`'s replica on
/// `rt` — the observation the sweep's derivation is asserted through.
async fn contact_present(rt: &Runtime, issuer: PdnId, device: NodeId) -> Result<bool> {
    Ok(rt.data().contacts_of(issuer).await?.contains(&device))
}

/// Poll until `rt` reads `expected` at `path` under `issuer`.
async fn claim_arrives(
    rt: &Runtime,
    issuer: PdnId,
    path: &EntryPath,
    expected: &[u8],
) -> Result<bool> {
    eventually(|| async {
        Ok(matches!(
            rt.data().read(issuer, path).await,
            Ok(Some(payload)) if payload == expected
        ))
    })
    .await
}

/// Tombstone `device`'s published record in the issuer's own store toward
/// `peer` — the act any device of the issuer performs. The probe links raw
/// and imports that store from the directory's write ticket, so the write
/// goes where the product's own withdrawal would go.
async fn withdraw_device_toward(
    node: &SyncNode,
    directory: &PrivateMetadataStore,
    peer: PdnId,
    device: NodeId,
) -> Result<()> {
    let own_kind = own_ticket_kind(&peer);
    // The ticket is accumulated inside the poll: a second read after it is
    // not the same read — a payload momentarily unfetchable reads as no
    // ticket at all, the very transient the poll exists for.
    let observed = RefCell::new(None);
    let arrived = eventually(|| async {
        let found = directory.get_ticket(&own_kind).await?;
        let seen = found.is_some();
        *observed.borrow_mut() = found;
        Ok(seen)
    })
    .await?;
    ensure!(
        arrived,
        "the pair's own-store ticket did not reach the probe's directory"
    );
    let own_ticket = observed
        .into_inner()
        .context("the poll reported the ticket and handed back nothing")?;
    ConnectionMetadataStore::import(node, own_ticket)
        .await?
        .withdraw_device(device)
        .await
}

/// Poll until the grant record of `issuer`'s data toward `peer` is live
/// and readable on `rt` — the record its classifier serves by. The
/// scenarios wait on this before shutting the publishing device down: the
/// record rides best-effort replication, and a publisher killed before it
/// crossed leaves the surviving device refusing the audience fail-closed.
async fn serving_ready(rt: &Runtime, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<bool> {
    eventually(|| async { rt.connections().grant_visible(identity, peer, issuer).await }).await
}

/// The core reachability property: the grant is published from the phone,
/// the laptop holds the granted claim by device replication, the phone
/// goes offline — and the audience still converges, on an update that
/// exists on the laptop alone.
///
/// Denied, existence hidden: the withheld claim never reaches the
/// audience; its view lists exactly the granted subset.
///
/// Denied, outsider: a runtime holding a ticket the laptop itself minted —
/// the same device that demonstrably serves the audience — obtains
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_audience_converges_from_a_device_that_did_not_publish_the_grant() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;
    let rt_carol = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;

    // The granted claim and a withheld one; the scoped grant covers the
    // first alone, published from the phone.
    let email = EntryPath::new("contact/email")?;
    let withheld = EntryPath::new("contact/phone")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    rt_phone
        .data()
        .write(alice, &withheld, b"+1-555-0100")
        .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;

    // Bob converges while the phone is up; the laptop holds the claim by
    // device replication; the sweep counts the laptop among Bob's contacts
    // for the granted replica — the route the rest of the scenario stands on.
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v1").await?,
        "the granted claim did not reach the audience while the phone was up"
    );
    assert!(
        claim_arrives(&rt_laptop, alice, &email, b"v1").await?,
        "the claim did not replicate to the laptop"
    );
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_bob, alice, laptop_id).await }).await?,
        "the issuer's other device never entered the audience replica's contacts"
    );
    assert!(
        serving_ready(&rt_laptop, alice, bob, alice).await?,
        "the grant record never reached the device that must serve by it"
    );

    // The phone goes offline; the update is written on the laptop alone.
    rt_phone.shutdown().await?;
    rt_laptop.data().write(alice, &email, b"v2").await?;

    // The audience converges from the device that did not publish the grant.
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v2").await?,
        "the audience did not converge from the issuer's other device"
    );

    // Existence hidden, after proven convergence: exactly the granted subset.
    assert!(rt_bob.data().read(alice, &withheld).await?.is_none());
    let listed: Vec<String> = rt_bob
        .data()
        .list(alice, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["contact/email".to_owned()],
        "the audience's view must contain exactly the granted subset"
    );

    // Denied, outsider: Carol aims a laptop-minted ticket at the very
    // device that just served Bob, and obtains nothing.
    let leaked = rt_laptop.data().share(alice, ShareMode::Read).await?;
    rt_carol.data().import_scoped(alice, leaked).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        rt_carol.data().list(alice, None).await?.is_empty(),
        "a bare ticket holder must get nothing from the serving sibling"
    );
    assert!(rt_carol.data().read(alice, &email).await?.is_none());

    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    rt_carol.shutdown().await?;
    Ok(())
}

/// No device is the founder: the grant is published from the *linked*
/// device, so the ticket names the laptop — and once the laptop goes
/// offline, the audience converges from the founder through the published
/// device set, exactly as it converges from a linked sibling when the
/// founder published.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_published_from_a_linked_device_reaches_past_it() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;

    // Establishment and the grant both run on the laptop; the phone never
    // touches the grant surface.
    let invite = rt_laptop.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_laptop, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_laptop.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_laptop,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;

    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v1").await?,
        "the granted claim did not reach the audience while the laptop was up"
    );
    assert!(
        claim_arrives(&rt_phone, alice, &email, b"v1").await?,
        "the claim did not replicate to the founder"
    );
    let phone_id = rt_phone.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_bob, alice, phone_id).await }).await?,
        "the founder never entered the audience replica's contacts"
    );
    assert!(
        serving_ready(&rt_phone, alice, bob, alice).await?,
        "the grant record never reached the device that must serve by it"
    );

    // The publishing device goes offline; the founder writes the update.
    rt_laptop.shutdown().await?;
    rt_phone.data().write(alice, &email, b"v2").await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v2").await?,
        "the audience did not converge from the founder past the publishing laptop"
    );

    rt_phone.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}

/// A device linked *after* the grant was published and consumed is dialed
/// too: its record replicates into the pair, the sweep counts it among the
/// audience replica's contacts — no re-import, no new grant — and the
/// audience converges from it once the publisher is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_the_import_is_dialed_too() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    // The whole grant flow completes with the issuer on one device.
    let alice = rt_phone.identity().create().await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v1").await?,
        "the granted claim did not reach the audience"
    );

    // Only now does the laptop join the identity. Its device record
    // replicates into the pair, and the audience's replica comes to count
    // it among its contacts — without a re-import and without a new grant.
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_bob, alice, laptop_id).await }).await?,
        "the late-linked device never entered the audience replica's contacts"
    );
    assert!(
        claim_arrives(&rt_laptop, alice, &email, b"v1").await?,
        "the claim did not replicate to the late-linked laptop"
    );
    assert!(
        serving_ready(&rt_laptop, alice, bob, alice).await?,
        "the grant record never reached the device that must serve by it"
    );

    // And it serves: the publisher goes offline, the update exists on the
    // late-linked device alone, the audience converges.
    rt_phone.shutdown().await?;
    rt_laptop.data().write(alice, &email, b"v2").await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v2").await?,
        "the audience did not converge from the late-linked device"
    );

    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}

/// A withdrawn device leaves the contact set: the issuer tombstones the
/// laptop's published record in the pair, and the next sweep's re-derived
/// set lacks it — while the still-published phone stays. The withdrawal is
/// written through the pair's own store from a linked probe, the same act
/// any device of the issuer performs.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_device_stops_being_a_contact() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;

    // Both issuer devices are contacts of the audience replica first — the
    // state the withdrawal must undo, not a set that never formed.
    let phone_id = rt_phone.node_id();
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async {
            Ok(contact_present(&rt_bob, alice, phone_id).await?
                && contact_present(&rt_bob, alice, laptop_id).await?)
        })
        .await?,
        "both issuer devices must be contacts before the withdrawal"
    );

    // The withdrawal: an alice device tombstones the laptop's published
    // record in the pair's own store.
    let (probe_node, probe_dir) = link_probe(&rt_phone, alice).await?;
    withdraw_device_toward(&probe_node, &probe_dir, bob, laptop_id).await?;

    // The re-derived set drops the withdrawn device and keeps the
    // published one.
    assert!(
        eventually(|| async {
            Ok(!contact_present(&rt_bob, alice, laptop_id).await?
                && contact_present(&rt_bob, alice, phone_id).await?)
        })
        .await?,
        "the withdrawn device did not leave the audience replica's contacts"
    );

    probe_node.shutdown().await?;
    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}

/// Two counterparties on one audience node: each granted replica keeps its
/// own contact set — a device the first peer publishes becomes a contact
/// of that peer's replica only, and never of the second peer's.
#[tokio::test(flavor = "multi_thread")]
async fn each_granted_replica_keeps_its_own_contact_set() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_carol = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    let carol = rt_carol.identity().create().await?;
    let bob = rt_bob.identity().create().await?;

    // Two connections, two grants, one audience node.
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let invite = rt_carol.connections().invite(carol, None).await?;
    establish_patiently(&rt_bob, bob, &rt_carol, carol, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"from-alice").await?;
    rt_carol.data().write(carol, &email, b"from-carol").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    granted_patiently(
        &rt_carol,
        carol,
        &rt_bob,
        bob,
        carol,
        common::claims_on(carol, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_bob, alice, &email, b"from-alice").await?);
    assert!(claim_arrives(&rt_bob, carol, &email, b"from-carol").await?);

    // Alice publishes a further device.
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_bob, alice, laptop_id).await }).await?,
        "the new device never entered its own peer's replica contacts"
    );

    // Scoped to the pair the grant came through: after the positive above,
    // the other peer's replica still knows nothing of it — and neither
    // replica names the other peer's device.
    assert!(
        !contact_present(&rt_bob, carol, laptop_id).await?,
        "another counterparty's device must not enter this replica's contacts"
    );
    assert!(!contact_present(&rt_bob, alice, rt_carol.node_id()).await?);

    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_carol.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}

/// Several identities on one node: two hosted audiences granted by the
/// same issuer bind one replica, so its contact set is the union of both
/// audiences' siblings — one binder's sweep must not strip the other
/// identity's devices from it. The union's boundary is asserted with it: a
/// third identity hosted on the same node holds no grant of this issuer,
/// and its device stays out of the set. That is the tightest unauthorized
/// party for a set keyed by issuer, per
/// `code-practices/access-control-tests.md` — a co-hosted identity, not an
/// outsider — and the only thing keeping it out is that the siblings come
/// from the pairs whose grant binds this issuer.
#[tokio::test(flavor = "multi_thread")]
async fn audiences_hosted_together_keep_both_sibling_sets() -> Result<()> {
    let rt_x = spawn_runtime().await?;
    let rt_second_of_x = spawn_runtime().await?;
    let rt_shared = spawn_runtime().await?;
    let rt_sibling_of_y = spawn_runtime().await?;
    let rt_sibling_of_z = spawn_runtime().await?;
    let rt_sibling_of_w = spawn_runtime().await?;

    // One node hosts both audience identities; each has a sibling device;
    // the issuer has a second device of its own. W is hosted beside them
    // and is granted nothing — the negative below is about its sibling.
    let x = rt_x.identity().create().await?;
    let y = rt_shared.identity().create().await?;
    let z = rt_shared.identity().create().await?;
    let w = rt_shared.identity().create().await?;
    link_patiently(&rt_second_of_x, &rt_x, x).await?;
    link_patiently(&rt_sibling_of_y, &rt_shared, y).await?;
    link_patiently(&rt_sibling_of_z, &rt_shared, z).await?;
    link_patiently(&rt_sibling_of_w, &rt_shared, w).await?;

    // The issuer grants both identities the same claim of one namespace.
    let invite = rt_x.connections().invite(x, None).await?;
    establish_patiently(&rt_shared, y, &rt_x, x, invite).await?;
    let invite = rt_x.connections().invite(x, None).await?;
    establish_patiently(&rt_shared, z, &rt_x, x, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_x.data().write(x, &email, b"v1").await?;
    granted_patiently(
        &rt_x,
        x,
        &rt_shared,
        y,
        x,
        common::claims_on(x, &email, false),
    )
    .await?;
    granted_patiently(
        &rt_x,
        x,
        &rt_shared,
        z,
        x,
        common::claims_on(x, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_shared, x, &email, b"v1").await?);

    // The one replica's contact set holds the issuer's devices — the
    // non-publishing one included — and *both* audiences' siblings at
    // once, and keeps holding them: with a per-identity set, each binder's
    // sweep would strip the other identity's sibling.
    let second_of_x = rt_second_of_x.node_id();
    let sibling_of_y = rt_sibling_of_y.node_id();
    let sibling_of_z = rt_sibling_of_z.node_id();
    assert!(
        eventually(|| async {
            Ok(contact_present(&rt_shared, x, second_of_x).await?
                && contact_present(&rt_shared, x, sibling_of_y).await?
                && contact_present(&rt_shared, x, sibling_of_z).await?)
        })
        .await?,
        "the shared replica's contacts must union the issuer's devices and both audiences' siblings"
    );

    // The boundary of that union, probed once the sweeps the positive
    // waited for have run: W is hosted on the same node and holds no grant
    // of X, so its sibling is no route to this replica and never enters the
    // set.
    assert!(
        !contact_present(&rt_shared, x, rt_sibling_of_w.node_id()).await?,
        "a co-hosted identity with no grant of this issuer must not lend its devices to the replica"
    );

    rt_x.shutdown().await?;
    rt_second_of_x.shutdown().await?;
    rt_shared.shutdown().await?;
    rt_sibling_of_y.shutdown().await?;
    rt_sibling_of_z.shutdown().await?;
    rt_sibling_of_w.shutdown().await?;
    Ok(())
}

/// The issuer half of the same union: two audiences of one issuer hosted
/// together share one replica, and its issuer devices are what *every*
/// bound pair publishes, not what the pair that swept last says alone. The
/// pairs replicate independently, so one pair's word taken as the whole set
/// would strip a device the issuer never withdrew.
///
/// Asserted where the two readings differ: the record is tombstoned in one
/// audience's pair only, and the device stays a contact because the other
/// audience's pair still publishes it. The control against a positive that
/// merely says "nothing ever leaves" is the second withdrawal — once no
/// bound pair publishes the device, it goes.
#[tokio::test(flavor = "multi_thread")]
async fn an_issuer_device_leaves_only_when_no_bound_pair_publishes_it() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_shared = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let y = rt_shared.identity().create().await?;
    let z = rt_shared.identity().create().await?;

    // One issuer, two audiences hosted together, a grant to each.
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, y, &rt_phone, alice, invite).await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, z, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        y,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        z,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_shared, alice, &email, b"v1").await?);

    // The laptop is a contact first — the state the withdrawals act on.
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_shared, alice, laptop_id).await }).await?,
        "the issuer's other device must be a contact before the withdrawals"
    );

    // Withdrawn in the pair toward Y alone: the pair toward Z still
    // publishes it, so it stays. Ordered by the two readings whose
    // divergence is the scenario's subject: first Y's pair demonstrably
    // stops publishing the device — the tombstone reached this node — then
    // a sweep of exactly that pair is run against the tombstoned state, and
    // only then the union is asserted to still carry the device. Without
    // the forced sweep the assertion would race the event-driven one and
    // could read a set derived before the tombstone.
    let (probe_node, probe_dir) = link_probe(&rt_phone, alice).await?;
    withdraw_device_toward(&probe_node, &probe_dir, y, laptop_id).await?;
    assert!(
        eventually(|| async {
            Ok(!rt_shared
                .connections()
                .published_devices_of(y, alice)
                .await?
                .contains(&laptop_id))
        })
        .await?,
        "the withdrawal toward Y never reached the audience node"
    );
    rt_shared.connections().sweep_pair_now(y, alice).await?;
    assert!(
        contact_present(&rt_shared, alice, laptop_id).await?,
        "a device withdrawn in one audience's pair must stay while another's pair publishes it"
    );

    // Withdrawn in the pair toward Z as well: no bound pair publishes it,
    // and it leaves.
    withdraw_device_toward(&probe_node, &probe_dir, z, laptop_id).await?;
    assert!(
        eventually(|| async { Ok(!contact_present(&rt_shared, alice, laptop_id).await?) }).await?,
        "the device did not leave once no bound pair published it"
    );

    probe_node.shutdown().await?;
    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_shared.shutdown().await?;
    Ok(())
}

/// Capabilities move: a grant is withdrawn — the binder forgets the
/// namespace — and granted anew over the same claim; the re-import derives
/// a fresh contact set, so the audience converges from the issuer's other
/// device again once the publisher is gone. The path worth testing is this
/// second grant, not the first.
#[tokio::test(flavor = "multi_thread")]
async fn a_regrant_after_withdrawal_rebuilds_the_contact_set() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_bob, alice, &email, b"v1").await?);

    // Withdrawal: the binder forgets what it imported — the issuer
    // resolves to nothing on the audience node again.
    rt_phone
        .connections()
        .withdraw_grant(alice, bob, alice)
        .await?;
    assert!(
        eventually(|| async {
            Ok(matches!(rt_bob.data().read(alice, &email).await,
                Err(err) if err.downcast_ref::<UnknownIssuer>().is_some()))
        })
        .await?,
        "the withdrawn namespace was still bound on the audience"
    );

    // The re-grant over the same claim: the binder imports afresh and the
    // sweep re-derives the contacts, the issuer's other device included.
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v1").await?,
        "the re-granted claim did not reach the audience"
    );
    let laptop_id = rt_laptop.node_id();
    assert!(
        eventually(|| async { contact_present(&rt_bob, alice, laptop_id).await }).await?,
        "the re-import did not rebuild the issuer-device contacts"
    );
    assert!(
        serving_ready(&rt_laptop, alice, bob, alice).await?,
        "the re-grant record never reached the device that must serve by it"
    );

    // And the rebuilt route serves: publisher off, update on the laptop
    // alone, the audience converges.
    rt_phone.shutdown().await?;
    rt_laptop.data().write(alice, &email, b"v2").await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v2").await?,
        "the audience did not converge from the sibling after the re-grant"
    );

    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}

/// One replica, two hosted audiences (ADR-0009): withdrawing the grant
/// toward one of them must not take the bytes from the other. The read
/// below is ordered by the binder's own record (`grant_bound`), not by
/// time — the unbind demonstrably ran before the survival is asserted. The
/// surviving audience then also receives a fresh write: the replica is not
/// merely present but still syncing. The denial half, per
/// `code-practices/access-control-tests.md`, is the second withdrawal: the
/// node that held two grants and lost both ends where an outsider starts —
/// the issuer resolves to nothing, no bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawal_toward_one_audience_spares_the_cohosted_other() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_shared = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    let y = rt_shared.identity().create().await?;
    let z = rt_shared.identity().create().await?;

    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, y, &rt_phone, alice, invite).await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, z, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        y,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        z,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_shared, alice, &email, b"v1").await?);

    // Both binders imported — the state the withdrawal acts on.
    assert!(
        eventually(|| async {
            Ok(rt_shared.connections().grant_bound(y, alice, alice).await
                && rt_shared.connections().grant_bound(z, alice, alice).await)
        })
        .await?,
        "both grants must be bound before the withdrawal"
    );

    // Withdrawn toward Y alone, asserted only once Y's binder has
    // demonstrably unbound.
    rt_phone
        .connections()
        .withdraw_grant(alice, y, alice)
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_shared.connections().grant_bound(y, alice, alice).await) })
            .await?,
        "the withdrawal toward Y was never processed"
    );
    assert_eq!(
        rt_shared.data().read(alice, &email).await?.as_deref(),
        Some(b"v1".as_slice()),
        "the shared replica must survive a withdrawal that leaves another grant standing"
    );
    rt_phone.data().write(alice, &email, b"v2").await?;
    assert!(
        claim_arrives(&rt_shared, alice, &email, b"v2").await?,
        "the surviving audience no longer converges"
    );

    // The last grant leaves, and the replica with it.
    rt_phone
        .connections()
        .withdraw_grant(alice, z, alice)
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_shared.connections().grant_bound(z, alice, alice).await) })
            .await?,
        "the withdrawal toward Z was never processed"
    );
    assert!(
        eventually(|| async {
            Ok(matches!(rt_shared.data().read(alice, &email).await,
                Err(err) if err.downcast_ref::<UnknownIssuer>().is_some()))
        })
        .await?,
        "the replica must leave with the last withdrawn grant"
    );

    rt_phone.shutdown().await?;
    rt_shared.shutdown().await?;
    Ok(())
}

/// The unbind decision is grounded in the durable grant records, not in
/// the binders' in-memory bookkeeping — which is exactly what a device
/// restart clears and rebuilds sweep by sweep (operating-conditions: the
/// device restarts). The record of Z's import is dropped by hand, the
/// restart-shaped arrangement, while Z's grant sits live and readable in
/// its pair; the withdrawal toward Y must find that grant and spare the
/// replica. A decision read off the bookkeeping alone counts zero holders
/// here and destroys it.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawal_counts_grants_not_bookkeeping() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_shared = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    let y = rt_shared.identity().create().await?;
    let z = rt_shared.identity().create().await?;

    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, y, &rt_phone, alice, invite).await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_shared, z, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        y,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_shared,
        z,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_shared, alice, &email, b"v1").await?);
    assert!(
        eventually(|| async {
            Ok(rt_shared.connections().grant_bound(y, alice, alice).await
                && rt_shared.connections().grant_bound(z, alice, alice).await)
        })
        .await?,
        "both grants must be bound before the arrangement"
    );

    // The in-memory record a restart clears: the replica and Z's durable
    // grant record both stand, the binder's memo of the import does not.
    // Z's binder sweeps only on its own pair's changes, and the withdrawal
    // below lands in Y's pair — nothing rebuilds the memo before the
    // decision it is cleared for.
    rt_shared
        .connections()
        .clear_grant_memo(z, alice, alice)
        .await;
    rt_phone
        .connections()
        .withdraw_grant(alice, y, alice)
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_shared.connections().grant_bound(y, alice, alice).await) })
            .await?,
        "the withdrawal toward Y was never processed"
    );
    assert_eq!(
        rt_shared.data().read(alice, &email).await?.as_deref(),
        Some(b"v1".as_slice()),
        "the unbind decision must find Z's durable grant with the bookkeeping empty"
    );

    rt_phone.shutdown().await?;
    rt_shared.shutdown().await?;
    Ok(())
}

/// The binder's memo is an optimization, the registry the arbiter: a
/// replica forgotten while the memo still names its import re-imports on
/// the pair's next sweep instead of being skipped forever. The desync is
/// hand-made (`forget_namespace` under `test-util`) as this test's subject
/// per `code-practices/product-path-arrangement` — the product paths keep
/// memo and registry together — and the recovery is asserted through the
/// product surface: the issuer republishes the grant, the sweep
/// re-imports, the entries return.
#[tokio::test(flavor = "multi_thread")]
async fn a_forgotten_replica_reimports_on_the_next_sweep() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone.data().write(alice, &email, b"v1").await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(claim_arrives(&rt_bob, alice, &email, b"v1").await?);

    // The desync: replica gone, memo still naming its import.
    rt_bob.data().forget_namespace(alice).await?;
    assert!(matches!(rt_bob.data().read(alice, &email).await,
        Err(err) if err.downcast_ref::<UnknownIssuer>().is_some()));
    assert!(
        rt_bob.connections().grant_bound(bob, alice, alice).await,
        "the memo must still name the import for the desync to be the one under test"
    );

    // The pair's next sweep re-imports — caused here by the issuer
    // republishing the same grant; any change of the pair's replica does.
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;
    assert!(
        claim_arrives(&rt_bob, alice, &email, b"v1").await?,
        "the memoized binding must not skip the re-import of a forgotten replica"
    );

    rt_phone.shutdown().await?;
    rt_bob.shutdown().await?;
    Ok(())
}
