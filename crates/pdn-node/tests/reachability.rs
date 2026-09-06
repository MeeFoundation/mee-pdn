//! A granted replica reaches every device of its issuer, not only the one
//! that published the grant; the scenarios turn the publishing device off
//! and require convergence from another, asserting the contact set through
//! the `test-util` surface rather than sleeping on it. Paired denial: the
//! serving sibling gives a bare ticket holder nothing. The grant sweep's
//! replica lifecycle (ADR-0009: one shared replica, last withdrawal takes
//! it) is asserted on the same surface. Compiles only under `test-util`.
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

const RECONCILE: Duration = Duration::from_millis(500);

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
/// `peer`, from a probe that imports the store from the directory's write
/// ticket — where the product's own withdrawal would go.
async fn withdraw_device_toward(
    node: &SyncNode,
    directory: &PrivateMetadataStore,
    peer: PdnId,
    device: NodeId,
) -> Result<()> {
    let own_kind = own_ticket_kind(&peer);
    // Accumulated inside the poll: a second read after it is not the same
    // read.
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

/// Poll until the grant record is readable on `rt` — waited on before the
/// publisher is shut down, since a publisher killed before the record
/// crossed leaves the surviving device refusing the audience fail-closed.
async fn serving_ready(rt: &Runtime, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<bool> {
    eventually(|| async {
        Ok(rt
            .connections()
            .read_own_grants(identity, peer)
            .await?
            .is_some_and(|grant| grant.issuer == issuer))
    })
    .await
}

/// The grant is published from the phone, the laptop holds the claim by
/// device replication, the phone goes offline, and the audience still
/// converges on an update that exists on the laptop alone. Denied: the
/// withheld claim never reaches the audience; Carol, aiming a laptop-minted
/// ticket at the very device that served Bob, obtains nothing.
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

    // The sweep counts the laptop among Bob's contacts — the route the rest
    // stands on.
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

    // Denied, outsider.
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

/// No device is the founder: the grant is published from the linked device,
/// so the ticket names the laptop, and the audience converges from the
/// founder through the published device set.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_published_from_a_linked_device_reaches_past_it() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;

    // The phone never touches the grant surface.
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

/// The connection's metadata pair is pointed at every device that holds it
/// — the identity's own devices and the peer's, re-derived per sweep — not
/// only at the devices its tickets name. The derivation is asserted rather
/// than the recovery it exists for: the engine's own recorded peers rescue
/// that recovery about 97 runs in 100
/// (`a_grant_published_by_a_lost_device_reaches_the_sibling_from_the_audience`
/// in `restart_recovery.rs`). Denied: a node that is a device of neither
/// side never enters the set.
#[tokio::test(flavor = "multi_thread")]
async fn the_metadata_pair_is_pointed_at_every_device_that_holds_it() -> Result<()> {
    let rt_phone = spawn_runtime().await?;
    let rt_laptop = spawn_runtime().await?;
    let rt_bob = spawn_runtime().await?;
    let rt_stranger = spawn_runtime().await?;

    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    // A node of its own, connected to nobody — the set must never name it.
    let stranger = rt_stranger.identity().create().await?;
    ensure!(stranger != alice, "the stranger must be its own identity");

    let phone_id = rt_phone.node_id();
    let bob_id = rt_bob.node_id();
    let stranger_id = rt_stranger.node_id();

    // The laptop opens the pair on its own sweep, and later sweeps take in
    // the device records as they replicate.
    assert!(
        eventually(|| async {
            let (own, peer) = rt_laptop.connections().pair_contacts(alice, bob).await?;
            Ok(own.contains(&bob_id) && own.contains(&phone_id) && peer.contains(&bob_id))
        })
        .await?,
        "the pair's halves never named the devices that hold them"
    );

    let (own, peer) = rt_laptop.connections().pair_contacts(alice, bob).await?;
    assert!(
        !own.contains(&stranger_id) && !peer.contains(&stranger_id),
        "a node that holds neither half must not be a contact of either"
    );

    // Denied, tighter than the stranger: the device that minted a half is
    // not a contact of it — a set that kept the ticket's own entry has the
    // node dialing itself once per reconcile. The positive beside it keeps
    // the denial from holding on a merely empty set.
    assert!(
        eventually(|| async {
            let (own, _peer) = rt_bob.connections().pair_contacts(bob, alice).await?;
            Ok(own.contains(&phone_id))
        })
        .await?,
        "bob's own half never named the issuer device that holds it"
    );
    let (bob_own, bob_peer) = rt_bob.connections().pair_contacts(bob, alice).await?;
    assert!(
        !bob_own.contains(&bob_id) && !bob_peer.contains(&bob_id),
        "the device that minted a half must not be a contact of it"
    );

    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    rt_stranger.shutdown().await?;
    Ok(())
}

/// A device linked after the grant was consumed is dialed too: no
/// re-import, no new grant.
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

    // Only now does the laptop join; its record replicates into the pair.
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

    // And it serves.
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

/// A withdrawn device leaves the contact set on the next sweep, while the
/// still-published phone stays. The withdrawal is written through the
/// pair's own store from a linked probe.
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

    // Both are contacts first — the state the withdrawal must undo.
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

    // The withdrawal.
    let (probe_node, probe_dir) = link_probe(&rt_phone, alice).await?;
    withdraw_device_toward(&probe_node, &probe_dir, bob, laptop_id).await?;

    // Dropped from the re-derived set; the published one stays.
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

/// Two counterparties on one audience node: a device the first peer
/// publishes becomes a contact of that peer's replica only.
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

    // Scoped to the pair the grant came through.
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

/// Two hosted audiences granted by one issuer bind one replica, so its
/// contact set is the union of both audiences' siblings — one binder's
/// sweep must not strip the other's devices. The boundary: a third
/// co-hosted identity holding no grant of this issuer (the tightest
/// unauthorized party for a set keyed by issuer) stays out.
#[tokio::test(flavor = "multi_thread")]
async fn audiences_hosted_together_keep_both_sibling_sets() -> Result<()> {
    let rt_x = spawn_runtime().await?;
    let rt_second_of_x = spawn_runtime().await?;
    let rt_shared = spawn_runtime().await?;
    let rt_sibling_of_y = spawn_runtime().await?;
    let rt_sibling_of_z = spawn_runtime().await?;
    let rt_sibling_of_w = spawn_runtime().await?;

    // W is hosted beside them and granted nothing.
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

    // Both audiences' siblings at once, and kept: a per-identity set would
    // let each binder's sweep strip the other's.
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

    // The boundary, probed after the sweeps the positive waited for.
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

/// An issuer device leaves the contact set only when no bound pair
/// publishes it: the replica's issuer devices are what every bound pair
/// publishes, not what the last-swept pair says alone, since pairs
/// replicate independently. Asserted where the readings differ: tombstoned
/// in one pair only, the device stays; withdrawn in both, it goes.
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

    // Withdrawn toward Y alone: Y's pair demonstrably stops publishing the
    // device, a sweep of exactly that pair runs against the tombstoned
    // state, and only then is the union asserted — otherwise the assertion
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

/// Withdrawn, then granted anew over the same claim: the re-import derives
/// a fresh contact set, so the audience converges from the issuer's other
/// device again. The second grant is the path under test.
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

    // Withdrawal: the binder forgets; the issuer resolves to nothing.
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

    // The re-grant: fresh import, contacts re-derived.
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

    // The rebuilt route serves.
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

/// One replica, two hosted audiences (ADR-0009): withdrawing toward one
/// must not take the bytes from the other. Ordered by the binder's own
/// record (`grant_bound`), not by time, and the survivor also receives a
/// fresh write. The denial is the second withdrawal: the node ends where an
/// outsider starts.
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

    // Asserted only once Y's binder has demonstrably unbound.
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

/// The unbind decision is grounded in the durable grant records, not the
/// binders' bookkeeping, which a restart clears. Z's import record is
/// dropped by hand while Z's grant sits live in its pair; the withdrawal
/// toward Y must find it and spare the replica.
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

    // The in-memory record a restart clears. Z's binder sweeps only on its
    // own pair's changes, and the withdrawal lands in Y's pair, so nothing
    // rebuilds the memo before the decision.
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

/// A replica forgotten while the binder's memo still names its import
/// re-imports on the pair's next sweep: the memo is an optimization, the
/// registry the arbiter. The desync is hand-made (`forget_namespace`,
/// `test-util`) as this test's subject; the recovery is asserted through
/// the product surface.
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

    // The next sweep re-imports — caused by the issuer republishing the same
    // grant.
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
