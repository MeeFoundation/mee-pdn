//! The connection metadata store at the data layer: dedicated replicas per
//! connection direction, the own→peer flip, grants riding inside (publish /
//! read / withdraw, payload-waiting), replication across both identities'
//! devices, last-writer-wins convergence — and the access pairs of
//! Invariant 3, each allowed path next to its tightest denial.
//!
//! Establishment (the pairing dialogue) lives in pdn-node; here the tickets
//! travel by direct handover, exactly the store-level acts the dialogue and
//! the directory perform. Registering the pair is what arms the ticket
//! bound Invariant 3 gives these stores, so every scenario arms the halves
//! it holds; a scenario that models one direction gets a locally created
//! stand-in for the other, which takes no part in what it asserts.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    claim_id_of, identity_of, AddrInfoOptions, ConnectionMetadataStore, Contact, DocTicket,
    EndpointId, GrantedClaim, PrivateMetadataStore, ReadGrant, ShareMode, SpawnOptions, SyncNode,
};
use pdn_types::{EntryPath, NodeId, NonEmpty, PdnId};
use test_utils::{eventually, host_identity, ids, memory_node, wait_entry_is};

/// A grantee replica has no gossip path, so every denial is "the reader
/// retried over several intervals and was refused" — milliseconds at this
/// cadence instead of the production default's tens of seconds.
const RECONCILE: Duration = Duration::from_millis(500);

async fn spawn_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// A real read ticket for grants to carry.
async fn data_ticket(node: &mut SyncNode, identity: PdnId, issuer: PdnId) -> Result<DocTicket> {
    node.create_namespace(identity, issuer).await?;
    node.share_ticket(
        identity,
        issuer,
        ShareMode::Read,
        AddrInfoOptions::RelayAndAddresses,
    )
    .await
}

/// Arm `own` as `identity`'s connection half toward `peer`, with a local
/// stand-in for the direction the scenario does not model.
async fn arm_own(
    node: &SyncNode,
    identity: PdnId,
    peer: PdnId,
    own: &ConnectionMetadataStore,
) -> Result<()> {
    let counterpart = ConnectionMetadataStore::create(node, identity).await?;
    node.host_connection(identity, peer, own, &counterpart)
}

/// Arm `peer_half` as the counterparty's direction of `identity`'s
/// connection toward `peer`, with a local stand-in for this side's own.
async fn arm_peer(
    node: &SyncNode,
    identity: PdnId,
    peer: PdnId,
    peer_half: &ConnectionMetadataStore,
) -> Result<()> {
    let own = ConnectionMetadataStore::create(node, identity).await?;
    node.host_connection(identity, peer, &own, peer_half)
}

/// The store carries a capability and never evaluates one, so one nominal
/// claim gives every record its shape.
// clippy.toml's expect relaxation reaches `#[test]` bodies only.
#[allow(clippy::expect_used)]
fn nominal_grant(issuer: PdnId, audience: PdnId) -> ReadGrant {
    let path = EntryPath::new("contact/email").expect("a valid path");
    ReadGrant {
        issuer,
        audience,
        claims: NonEmpty::new(GrantedClaim {
            claim: claim_id_of(&issuer, &path),
            write: false,
        }),
    }
}

/// Wait until `store` reads the grant for `issuer` as exactly `expected`.
async fn wait_grant_is(
    store: &ConnectionMetadataStore,
    issuer: PdnId,
    audience: PdnId,
    expected: &DocTicket,
) -> Result<bool> {
    let expected = expected.to_string();
    eventually(|| async {
        Ok(store
            .read_grant(issuer, audience)
            .await?
            .granted()
            .is_some_and(|(_cap, ticket)| ticket.to_string() == expected))
    })
    .await
}

/// One replica per direction, the own→peer flip, and an import that binds
/// before content: the handle reads absent at once and converges without
/// re-import.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, both directions and the isolation in one place
async fn dedicated_replicas_own_peer_flip_and_isolation() -> Result<()> {
    let mut alice = memory_node().await?;
    let bob = memory_node().await?;
    let carol = memory_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let _bob_dir = host_identity(&bob, ids::BOB).await?;
    let _carol_dir = host_identity(&carol, ids::CAROL).await?;

    // Alice issues one store per counterparty; Bob issues one toward Alice.
    let a_own_b = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    let a_own_c = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    let b_own_a = ConnectionMetadataStore::create(&bob, ids::BOB).await?;
    arm_own(&alice, ids::ALICE, ids::BOB, &a_own_b).await?;
    arm_own(&alice, ids::ALICE, ids::CAROL, &a_own_c).await?;

    // Every direction is its own replica: all three namespaces differ.
    let ns_toward_bob = a_own_b
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?
        .capability
        .id();
    let ns_toward_carol = a_own_c
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?
        .capability
        .id();
    let ns_from_bob = b_own_a
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?
        .capability
        .id();
    assert_ne!(
        ns_toward_bob, ns_from_bob,
        "the two directions must be distinct replicas"
    );
    assert_ne!(
        ns_toward_bob, ns_toward_carol,
        "stores toward different peers must be distinct replicas"
    );

    // The counterparties import the read tickets — before any content
    // exists, so the absent read below is deterministic.
    let b_peer_a = ConnectionMetadataStore::import(
        &bob,
        ids::BOB,
        a_own_b
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    bob.host_connection(ids::BOB, ids::ALICE, &b_own_a, &b_peer_a)?;
    let c_peer_a = ConnectionMetadataStore::import(
        &carol,
        ids::CAROL,
        a_own_c
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    arm_peer(&carol, ids::CAROL, ids::ALICE, &c_peer_a).await?;

    // Import binds before content arrives: the handle is usable at once and
    // reads return absent — nothing has been published yet.
    assert!(b_peer_a
        .read_grant(ids::ALICE, ids::BOB)
        .await?
        .granted()
        .is_none());
    assert!(b_peer_a.list_grants().await?.is_empty());

    // Alice grants her data store toward Bob and a second one toward Carol.
    let ticket_for_bob = data_ticket(&mut alice, ids::ALICE, ids::ALICE).await?;
    let ticket_for_carol = data_ticket(&mut alice, ids::ALICE, ids::ALICE_AT_WORK).await?;
    a_own_b
        .publish_grant(&nominal_grant(ids::ALICE, ids::BOB), &ticket_for_bob)
        .await?;
    a_own_c
        .publish_grant(
            &nominal_grant(ids::ALICE_AT_WORK, ids::CAROL),
            &ticket_for_carol,
        )
        .await?;

    // The own→peer flip: the entry written into `own` is read from the
    // counterpart's `peer` — the same replica at both sides, no re-import.
    assert!(
        wait_grant_is(&b_peer_a, ids::ALICE, ids::BOB, &ticket_for_bob).await?,
        "grant did not converge from Alice's own store to Bob's peer store"
    );
    assert!(
        wait_grant_is(&c_peer_a, ids::ALICE_AT_WORK, ids::CAROL, &ticket_for_carol).await?,
        "grant did not converge from Alice's own store to Carol's peer store"
    );

    // Per-connection isolation, probed after both replicas demonstrably
    // converged: the grant toward Bob never appears in Carol's store, and
    // vice versa — and nothing of Alice's stores leaked into Bob's own
    // reverse-direction replica.
    assert_eq!(c_peer_a.list_grants().await?, vec![ids::ALICE_AT_WORK]);
    assert!(c_peer_a
        .read_grant(ids::ALICE, ids::CAROL)
        .await?
        .granted()
        .is_none());
    assert_eq!(b_peer_a.list_grants().await?, vec![ids::ALICE]);
    assert!(b_own_a.list_grants().await?.is_empty());

    alice.shutdown().await?;
    bob.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}

/// Grants over the pair's lifetime, across both identities' devices: a
/// round-trip, a grant published long after the exchange with no new
/// tickets, a withdrawal that reads absent everywhere, and concurrent
/// updates of one grant key converging to a single entry. A grant lists as
/// soon as its record syncs and reads absent until its payload arrives; the
/// polls ride that contract.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the pair's whole lifetime in one place
async fn grants_replicate_withdraw_and_converge_across_devices() -> Result<()> {
    let mut a_phone = memory_node().await?;
    let mut a_laptop = memory_node().await?;
    let b_phone = memory_node().await?;
    let b_laptop = memory_node().await?;
    let _a_phone_dir = host_identity(&a_phone, ids::ALICE).await?;
    let _a_laptop_dir = host_identity(&a_laptop, ids::ALICE).await?;
    let _b_phone_dir = host_identity(&b_phone, ids::BOB).await?;
    let _b_laptop_dir = host_identity(&b_laptop, ids::BOB).await?;

    // The laptop opens from the write ticket, Bob's devices from the read
    // ticket.
    let own_phone = ConnectionMetadataStore::create(&a_phone, ids::ALICE).await?;
    arm_own(&a_phone, ids::ALICE, ids::BOB, &own_phone).await?;
    let write_ticket = own_phone
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let read_ticket = own_phone
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let own_laptop = ConnectionMetadataStore::import(&a_laptop, ids::ALICE, write_ticket).await?;
    arm_own(&a_laptop, ids::ALICE, ids::BOB, &own_laptop).await?;
    let peer_b_phone =
        ConnectionMetadataStore::import(&b_phone, ids::BOB, read_ticket.clone()).await?;
    arm_peer(&b_phone, ids::BOB, ids::ALICE, &peer_b_phone).await?;
    let peer_b_laptop = ConnectionMetadataStore::import(&b_laptop, ids::BOB, read_ticket).await?;
    arm_peer(&b_laptop, ids::BOB, ids::ALICE, &peer_b_laptop).await?;

    // Written on the issuer's phone, read on the issuer's laptop and on
    // both of the counterparty's devices.
    let first = data_ticket(&mut a_phone, ids::ALICE, ids::ALICE).await?;
    own_phone
        .publish_grant(&nominal_grant(ids::ALICE, ids::BOB), &first)
        .await?;
    for (name, store) in [
        ("issuer's laptop", &own_laptop),
        ("counterparty's phone", &peer_b_phone),
        ("counterparty's laptop", &peer_b_laptop),
    ] {
        assert!(
            wait_grant_is(store, ids::ALICE, ids::BOB, &first).await?,
            "grant did not converge to the {name}"
        );
    }

    // A grant published after the exchange crosses with no new tickets
    // handed over — the channel outlives the pairing moment.
    let second = data_ticket(&mut a_phone, ids::ALICE, ids::ALICE_AT_WORK).await?;
    own_phone
        .publish_grant(&nominal_grant(ids::ALICE_AT_WORK, ids::BOB), &second)
        .await?;
    assert!(
        wait_grant_is(&peer_b_phone, ids::ALICE_AT_WORK, ids::BOB, &second).await?,
        "a later grant did not reach the counterparty over the existing pair"
    );

    // Withdrawal: the tombstone replicates and the grant reads as absent —
    // and stops listing — on the counterparty.
    own_phone.withdraw_grant(ids::ALICE).await?;
    assert!(
        eventually(|| async {
            Ok(peer_b_phone
                .read_grant(ids::ALICE, ids::BOB)
                .await?
                .granted()
                .is_none())
        })
        .await?,
        "withdrawn grant still reads on the counterparty"
    );
    assert!(
        eventually(|| async { Ok(!peer_b_phone.list_grants().await?.contains(&ids::ALICE)) })
            .await?,
        "withdrawn grant still lists on the counterparty"
    );

    // Concurrent updates of one grant key from the issuer's two devices:
    // every device of both identities resolves to the same single entry.
    let from_phone = data_ticket(&mut a_phone, ids::ALICE, ids::ALICE_AT_LEISURE).await?;
    let from_laptop = data_ticket(&mut a_laptop, ids::ALICE, ids::ALICE_AT_LEISURE).await?;
    own_phone
        .publish_grant(&nominal_grant(ids::ALICE_AT_LEISURE, ids::BOB), &from_phone)
        .await?;
    own_laptop
        .publish_grant(
            &nominal_grant(ids::ALICE_AT_LEISURE, ids::BOB),
            &from_laptop,
        )
        .await?;
    let stores = [&own_phone, &own_laptop, &peer_b_phone, &peer_b_laptop];
    assert!(
        eventually(|| async {
            let mut seen = Vec::new();
            for store in stores {
                match store
                    .read_grant(ids::ALICE_AT_LEISURE, ids::BOB)
                    .await?
                    .granted()
                {
                    Some((_cap, ticket)) => seen.push(ticket.to_string()),
                    None => return Ok(false),
                }
            }
            let all_equal = seen.windows(2).all(|w| w[0] == w[1]);
            let is_one_of_the_writes = seen
                .first()
                .is_some_and(|t| *t == from_phone.to_string() || *t == from_laptop.to_string());
            Ok(all_equal && is_one_of_the_writes)
        })
        .await?,
        "concurrent grant updates did not converge to one entry on every device"
    );

    a_phone.shutdown().await?;
    a_laptop.shutdown().await?;
    b_phone.shutdown().await?;
    b_laptop.shutdown().await?;
    Ok(())
}

/// A republication replaces the grant record in one write and a withdrawal
/// removes it in one tombstone: one record per issuer, so a half-replaced
/// or half-withdrawn state is unrepresentable.
#[tokio::test(flavor = "multi_thread")]
async fn one_grant_record_replaces_and_withdraws_atomically() -> Result<()> {
    let mut alice = memory_node().await?;
    let bob = memory_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let _bob_dir = host_identity(&bob, ids::BOB).await?;

    // Alice's own store toward Bob; Bob imports the read ticket.
    let own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    arm_own(&alice, ids::ALICE, ids::BOB, &own).await?;
    let b_peer = ConnectionMetadataStore::import(
        &bob,
        ids::BOB,
        own.share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    arm_peer(&bob, ids::BOB, ids::ALICE, &b_peer).await?;

    let ticket = data_ticket(&mut alice, ids::ALICE, ids::ALICE).await?;
    let email = EntryPath::new("contact/email")?;
    let grant = ReadGrant {
        issuer: ids::ALICE,
        audience: ids::BOB,
        claims: NonEmpty::new(GrantedClaim {
            claim: claim_id_of(&ids::ALICE, &email),
            write: false,
        }),
    };

    // Published: the counterparty reads the capability and its ticket.
    own.publish_grant(&grant, &ticket).await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE, ids::BOB, &ticket).await?,
        "the grant did not converge to the counterparty"
    );

    // Republished onto a second replica: the one record is replaced wholesale.
    let replacement = data_ticket(&mut alice, ids::ALICE, ids::ALICE_AT_WORK).await?;
    own.publish_grant(&grant, &replacement).await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE, ids::BOB, &replacement).await?,
        "the republished grant did not take effect on the counterparty"
    );

    // Withdraw: one tombstone removes the grant — absent and unlisted, on
    // both sides.
    own.withdraw_grant(ids::ALICE).await?;
    assert!(own
        .read_grant(ids::ALICE, ids::BOB)
        .await?
        .granted()
        .is_none());
    assert!(
        eventually(|| async {
            Ok(b_peer
                .read_grant(ids::ALICE, ids::BOB)
                .await?
                .granted()
                .is_none()
                && !b_peer.list_grants().await?.contains(&ids::ALICE))
        })
        .await?,
        "the withdrawal did not converge to the counterparty"
    );

    alice.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}

/// A data replica refuses to be opened as a device-shared store; beside it,
/// the directory this node created opens. `PrivateMetadataStore::open`
/// enrols what it opens in the gossip swarm, so an open aimed at a data
/// namespace would silently widen the data path of a grantee import.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_replica_refuses_a_device_shared_open() -> Result<()> {
    let alice = memory_node().await?;
    let directory = host_identity(&alice, ids::ALICE).await?;
    alice.create_namespace(ids::ALICE, ids::ALICE).await?;
    let data = alice
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Read,
            AddrInfoOptions::Addresses,
        )
        .await?;

    assert!(
        PrivateMetadataStore::open(&alice, ids::ALICE, data.capability.id())
            .await
            .is_err(),
        "a data replica must not open as a device-shared store"
    );
    let reopened = PrivateMetadataStore::open(&alice, ids::ALICE, directory.namespace())
        .await?
        .expect("the node holds its own directory replica");
    assert!(
        reopened.list_devices().await?.contains(&alice.node_id()),
        "the directory itself must still open and read back"
    );

    alice.shutdown().await?;
    Ok(())
}

/// A device-shared replica refuses a data import, and stays writable
/// through its own surface. Honoring the import would overwrite the store's
/// tracking on the word of whoever minted the ticket, and the grantee
/// downgrade would cut its live path by leaving the gossip swarm.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_shared_replica_refuses_a_data_import() -> Result<()> {
    let alice = memory_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    let ticket = own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;

    assert!(
        alice
            .import_namespace_scoped(ids::ALICE, ids::BOB, ticket.clone())
            .await
            .is_err(),
        "a scoped data import must refuse a device-shared replica's namespace"
    );
    assert!(
        alice
            .import_namespace(ids::ALICE, ids::BOB, ticket)
            .await
            .is_err(),
        "a device data import must refuse a device-shared replica's namespace"
    );
    // The store is untouched: still writable through its own surface.
    own.publish_device(alice.node_id()).await?;
    assert_eq!(own.published_devices().await?, vec![alice.node_id()]);

    alice.shutdown().await?;
    Ok(())
}

/// A grant record naming a foreign issuer over a namespace the identity
/// already holds for itself is refused before it changes anything: the
/// import is rejected and the replica keeps the contacts it was reconciling
/// by. The registry refuses a second issuer on its own, but only after the
/// tracking entry — keyed by namespace and overwritten blind — is already
/// gone, and nothing puts it back.
///
/// Denied: both import paths, the grantee one and the device one, each
/// naming an issuer the namespace is not bound to.
///
/// The ticket is minted here rather than carried by a grant record because
/// a counterparty choosing what the record points at is the subject: the
/// namespace id of any grant an identity ever published is known to its
/// audience, and a read capability is that id.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_naming_another_issuer_is_refused_before_it_rewrites_tracking() -> Result<()> {
    let mut alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let own = data_ticket(&mut alice, ids::ALICE, ids::ALICE).await?;

    // A sibling device of Alice's own, as the product's contacts name one.
    let sibling_node = spawn_node().await?;
    let sibling = Contact::new(sibling_node.dial_handle().addr(), identity_of(ids::ALICE));
    alice.set_namespace_contacts(ids::ALICE, ids::ALICE, vec![sibling.clone()])?;

    // What a counterparty publishes: a ticket on Alice's own namespace,
    // carrying its own addressing, under a record naming a third issuer.
    let counterparty = spawn_node().await?;
    let mut planted = own.clone();
    planted.nodes = vec![counterparty.dial_handle().addr()];

    for (path, refused) in [
        (
            "grantee",
            alice
                .import_namespace_scoped(ids::ALICE, ids::CAROL, planted.clone())
                .await,
        ),
        (
            "device",
            alice
                .import_namespace(ids::ALICE, ids::CAROL, planted.clone())
                .await,
        ),
    ] {
        // Denied (a record naming an issuer the namespace is not bound to).
        assert!(
            refused.is_err(),
            "the {path} import took a namespace bound to another issuer"
        );
    }

    assert_eq!(
        alice.namespace_contacts(ids::ALICE, ids::ALICE)?,
        vec![sibling],
        "the refused import rewrote the contacts of the identity's own replica"
    );
    assert_eq!(
        alice.data_namespace_of(ids::ALICE, ids::ALICE)?,
        Some(own.capability.id()),
        "the refused import moved the identity's own binding"
    );
    assert!(
        alice.data_namespace_of(ids::ALICE, ids::CAROL)?.is_none(),
        "the refused import registered the issuer it named"
    );

    alice.shutdown().await?;
    sibling_node.shutdown().await?;
    counterparty.shutdown().await?;
    Ok(())
}

/// A ticket naming a namespace the identity already holds in another role
/// is refused as a device-shared import, and a namespace it holds in none
/// is imported as before. A device-shared store is served on its ticket
/// alone (Invariants 1 and 3), so honoring such a ticket would hand the
/// replica over entire, past the grant that bounds it.
///
/// Denied: three roles in turn — the identity's own data replica, a
/// replica it holds under a grant, and its directory — each offered the
/// way a counterparty offers its connection metadata store.
///
/// The tickets are minted here rather than handed over by a ceremony
/// because a counterparty choosing the namespace is the subject: a read
/// capability is the namespace id, so anyone who learns one can mint the
/// ticket this test refuses.
#[tokio::test(flavor = "multi_thread")]
async fn a_namespace_held_in_another_role_refuses_a_device_shared_import() -> Result<()> {
    let mut alice = spawn_node().await?;
    let directory = host_identity(&alice, ids::ALICE).await?;

    // The identity's own data replica, and one it holds under a grant.
    let own_data = data_ticket(&mut alice, ids::ALICE, ids::ALICE).await?;
    let mut bob = spawn_node().await?;
    let _bob_dir = host_identity(&bob, ids::BOB).await?;
    let bobs_data = data_ticket(&mut bob, ids::BOB, ids::BOB).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, bobs_data.clone())
        .await?;

    let directory_ticket = directory
        .share_ticket(ShareMode::Read, AddrInfoOptions::Addresses)
        .await?;

    let own_data_namespace = own_data.capability.id();
    for (role, ticket) in [
        ("its own data replica", own_data),
        ("a replica held under a grant", bobs_data),
        ("its directory", directory_ticket),
    ] {
        // Denied (a counterparty naming a namespace already in use).
        assert!(
            ConnectionMetadataStore::import(&alice, ids::ALICE, ticket)
                .await
                .is_err(),
            "{role} was repurposed as a device-shared store"
        );
    }

    // Allowed: a namespace in no other role imports as before, and the
    // data replica the first denial protected still reads back.
    let fresh = ConnectionMetadataStore::create(&bob, ids::BOB).await?;
    let imported = ConnectionMetadataStore::import(
        &alice,
        ids::ALICE,
        fresh
            .share_ticket(ShareMode::Read, AddrInfoOptions::Addresses)
            .await?,
    )
    .await?;
    assert_eq!(
        imported.namespace(),
        fresh.namespace(),
        "a namespace held in no other role must import as the counterparty's store"
    );
    assert_eq!(
        alice.data_namespace_of(ids::ALICE, ids::ALICE)?,
        Some(own_data_namespace),
        "the identity's data replica lost its binding to a refused import"
    );

    alice.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}

/// Opening a pair does not resurrect a withdrawn device record: a first
/// touch publishes, a tombstone holds, and deliberate re-assertion
/// (`publish_device`) is a distinct act. The tombstone is an agreement
/// honest devices keep; this pins that they keep it by default.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_device_record_is_not_resurrected_by_pair_opening() -> Result<()> {
    let alice = memory_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    let device = alice.node_id();

    // First touch publishes.
    own.ensure_device_published(device).await?;
    assert_eq!(own.published_devices().await?, vec![device]);

    // Withdrawn, then re-opened: the tombstone holds.
    own.withdraw_device(device).await?;
    assert!(own.published_devices().await?.is_empty());
    own.ensure_device_published(device).await?;
    assert!(
        own.published_devices().await?.is_empty(),
        "opening a pair must not re-assert a withdrawn device record"
    );

    // Deliberate re-assertion is a distinct act and still works.
    own.publish_device(device).await?;
    assert_eq!(own.published_devices().await?, vec![device]);

    alice.shutdown().await?;
    Ok(())
}

/// A `devices/` key that decodes into no node id withholds itself and never
/// the set: consumers convert the set into endpoint ids without error
/// handling, so one garbage record would otherwise cost every audience its
/// whole contact set. The record is written through the product surface
/// itself — `publish_device` accepts the opaque id — so the denial is the
/// boundary's, not the writer's.
#[tokio::test(flavor = "multi_thread")]
async fn a_garbage_device_key_withholds_itself_not_the_set() -> Result<()> {
    let alice = memory_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;

    let device = alice.node_id();
    own.publish_device(device).await?;
    // The first constant-byte pattern that decompresses into no curve
    // point — deterministic, and the search ends within a few steps.
    let garbage = (0u8..=255)
        .map(|byte| [byte; 32])
        .find(|bytes| EndpointId::from_bytes(bytes).is_err())
        .expect("half of all byte strings are off-curve");
    own.publish_device(NodeId::from_bytes(garbage)).await?;

    assert_eq!(
        own.published_devices().await?,
        vec![device],
        "an unresolvable device record must be withheld, and the resolvable one kept"
    );

    alice.shutdown().await?;
    Ok(())
}

/// The access pairs of Invariant 3. Write: the issuer's second device
/// writes; the counterparty, on the read ticket, cannot and creates no
/// entry. Read: the counterparty reads the whole store; a third identity
/// sharing its own pair with the issuer reads nothing that reveals this one.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn issuer_devices_write_counterparty_reads_third_party_observes_nothing() -> Result<()> {
    let mut a_phone = memory_node().await?;
    let mut a_laptop = memory_node().await?;
    let mut bob = memory_node().await?;
    let carol = memory_node().await?;
    let _a_phone_dir = host_identity(&a_phone, ids::ALICE).await?;
    let _a_laptop_dir = host_identity(&a_laptop, ids::ALICE).await?;
    let _bob_dir = host_identity(&bob, ids::BOB).await?;
    let _carol_dir = host_identity(&carol, ids::CAROL).await?;

    // The A→B pair: laptop on the write ticket, Bob on the read ticket.
    let own_b_phone = ConnectionMetadataStore::create(&a_phone, ids::ALICE).await?;
    arm_own(&a_phone, ids::ALICE, ids::BOB, &own_b_phone).await?;
    let own_b_laptop = ConnectionMetadataStore::import(
        &a_laptop,
        ids::ALICE,
        own_b_phone
            .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    arm_own(&a_laptop, ids::ALICE, ids::BOB, &own_b_laptop).await?;
    let b_peer = ConnectionMetadataStore::import(
        &bob,
        ids::BOB,
        own_b_phone
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    arm_peer(&bob, ids::BOB, ids::ALICE, &b_peer).await?;

    // Carol shares state with Alice too — her own pair, a distinct replica.
    let own_toward_carol = ConnectionMetadataStore::create(&a_phone, ids::ALICE).await?;
    arm_own(&a_phone, ids::ALICE, ids::CAROL, &own_toward_carol).await?;
    let c_peer = ConnectionMetadataStore::import(
        &carol,
        ids::CAROL,
        own_toward_carol
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    arm_peer(&carol, ids::CAROL, ids::ALICE, &c_peer).await?;

    // Allowed: the issuer's second device writes, the counterparty reads.
    let from_laptop = data_ticket(&mut a_laptop, ids::ALICE, ids::ALICE_AT_WORK).await?;
    own_b_laptop
        .publish_grant(&nominal_grant(ids::ALICE_AT_WORK, ids::BOB), &from_laptop)
        .await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE_AT_WORK, ids::BOB, &from_laptop).await?,
        "the issuer's second device's write did not reach the counterparty"
    );

    // Denied: the counterparty holds only the read ticket — its write is
    // refused outright.
    let bob_ticket = data_ticket(&mut bob, ids::BOB, ids::BOB).await?;
    assert!(
        b_peer
            .publish_grant(&nominal_grant(ids::BOB, ids::ALICE), &bob_ticket)
            .await
            .is_err(),
        "a write through a read-only ticket must be refused"
    );

    // ...and created no entry: a later legitimate write converges — so
    // replication demonstrably flowed after the refusal — while the refused
    // key reads absent at the issuer and at the counterparty itself.
    let sentinel = data_ticket(&mut a_phone, ids::ALICE, ids::ALICE).await?;
    own_b_phone
        .publish_grant(&nominal_grant(ids::ALICE, ids::BOB), &sentinel)
        .await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE, ids::BOB, &sentinel).await?,
        "the sentinel grant did not converge after the refused write"
    );
    assert!(own_b_phone
        .read_grant(ids::BOB, ids::ALICE)
        .await?
        .granted()
        .is_none());
    assert!(b_peer
        .read_grant(ids::BOB, ids::BOB)
        .await?
        .granted()
        .is_none());

    // Denied: Carol's store carries exactly what Alice granted her, none of
    // Bob's grants.
    let for_carol = data_ticket(&mut a_phone, ids::ALICE, ids::ALICE_AT_LEISURE).await?;
    own_toward_carol
        .publish_grant(
            &nominal_grant(ids::ALICE_AT_LEISURE, ids::CAROL),
            &for_carol,
        )
        .await?;
    assert!(
        wait_grant_is(&c_peer, ids::ALICE_AT_LEISURE, ids::CAROL, &for_carol).await?,
        "Alice's grant toward Carol did not converge"
    );
    let ns_toward_bob = own_b_phone
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?
        .capability
        .id();
    let ns_toward_carol = own_toward_carol
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?
        .capability
        .id();
    assert_ne!(
        ns_toward_bob, ns_toward_carol,
        "Carol's pair must be a distinct replica"
    );
    assert_eq!(c_peer.list_grants().await?, vec![ids::ALICE_AT_LEISURE]);
    assert!(c_peer
        .read_grant(ids::ALICE, ids::CAROL)
        .await?
        .granted()
        .is_none());
    assert!(c_peer
        .read_grant(ids::ALICE_AT_WORK, ids::CAROL)
        .await?
        .granted()
        .is_none());

    a_phone.shutdown().await?;
    a_laptop.shutdown().await?;
    bob.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}

/// The sibling path preserves the issuer's scope, refuses a device listed
/// only in a co-located identity's directory, and honors the withdrawal
/// from the next session while retaining what was delivered.
///
/// Denied: the intruder resolves in the co-located identity's directory
/// alone, so the phone refuses it although it holds the replica; and
/// after the withdrawal neither device advances. Every denial is ordered
/// after a write that demonstrably reached the audience.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn a_sibling_session_keeps_scope_withdrawal_and_audience() -> Result<()> {
    let a_phone = spawn_node().await?;
    let a_laptop = spawn_node().await?;
    let intruder = spawn_node().await?;
    let mut bob = spawn_node().await?;

    // Alice's directory lists her devices. A co-located second identity's
    // directory — hosted on the same phone — lists the intruder.
    let directory = host_identity(&a_phone, ids::ALICE).await?;
    directory.add_device(a_laptop.node_id()).await?;
    let leisure_dir = host_identity(&a_phone, ids::ALICE_AT_LEISURE).await?;
    leisure_dir.add_device(intruder.node_id()).await?;
    let laptop_dir = host_identity(&a_laptop, ids::ALICE).await?;
    laptop_dir.add_device(a_phone.node_id()).await?;
    let _intruder_dir = host_identity(&intruder, ids::ALICE_AT_LEISURE).await?;

    // Bob's namespace holds a granted claim and a withheld one; his store
    // toward Alice carries a scoped grant on the granted claim alone.
    let _bob_dir = host_identity(&bob, ids::BOB).await?;
    let email = EntryPath::new("contact/email")?;
    let withheld = EntryPath::new("contact/phone")?;
    let data_read = data_ticket(&mut bob, ids::BOB, ids::BOB).await?;
    let author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@example.org")
        .await?;
    bob.write(ids::BOB, ids::BOB, author, &withheld, b"+1-555-0100")
        .await?;
    let b_own = ConnectionMetadataStore::create(&bob, ids::BOB).await?;
    b_own.publish_device(bob.node_id()).await?;
    let grant = ReadGrant {
        issuer: ids::BOB,
        audience: ids::ALICE,
        claims: NonEmpty::new(GrantedClaim {
            claim: claim_id_of(&ids::BOB, &email),
            write: false,
        }),
    };
    b_own.publish_grant(&grant, &data_read).await?;

    // The pair, as establishment leaves it: Alice publishes the phone in
    // her own half, which is what makes Bob serve the phone at all.
    let a_own = ConnectionMetadataStore::create(&a_phone, ids::ALICE).await?;
    a_own.publish_device(a_phone.node_id()).await?;
    let phone_peer = ConnectionMetadataStore::import(
        &a_phone,
        ids::ALICE,
        b_own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    a_phone.host_connection(ids::ALICE, ids::BOB, &a_own, &phone_peer)?;
    let bob_peer = ConnectionMetadataStore::import(
        &bob,
        ids::BOB,
        a_own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    bob.host_connection(ids::BOB, ids::ALICE, &b_own, &bob_peer)?;
    assert!(
        eventually(|| async {
            Ok(phone_peer
                .read_grant(ids::BOB, ids::ALICE)
                .await?
                .granted()
                .is_some())
        })
        .await?,
        "the scoped grant did not reach the phone"
    );

    // Allowed: the phone receives exactly the granted claim from the
    // armed issuer.
    a_phone
        .import_namespace_scoped(ids::ALICE, ids::BOB, data_read.clone())
        .await?;
    assert!(
        wait_entry_is(&a_phone, ids::ALICE, ids::BOB, &email, b"bob@example.org").await?,
        "the granted entry did not reach the phone"
    );

    // The laptop's only contact is the phone: the sibling session serves
    // exactly the claim set. Bob's ticket is aimed at the phone by hand,
    // since a grantee mints none.
    let mut phone_ticket = data_read.clone();
    phone_ticket.nodes = vec![a_phone.dial_handle().addr()];
    phone_ticket.identity = identity_of(ids::ALICE);
    a_laptop
        .import_namespace_scoped(ids::ALICE, ids::BOB, phone_ticket.clone())
        .await?;
    assert!(
        wait_entry_is(&a_laptop, ids::ALICE, ids::BOB, &email, b"bob@example.org").await?,
        "the granted entry did not reach the laptop through the sibling"
    );

    // A proven second wave through the same sibling: the update arrives,
    // the withheld entry still does not — hidden, not merely late.
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@new.example.org")
        .await?;
    assert!(
        wait_entry_is(
            &a_laptop,
            ids::ALICE,
            ids::BOB,
            &email,
            b"bob@new.example.org"
        )
        .await?,
        "the granted update did not reach the laptop through the sibling"
    );
    assert!(a_laptop
        .read(ids::ALICE, ids::BOB, &withheld)
        .await?
        .is_none());
    assert!(a_phone
        .read(ids::ALICE, ids::BOB, &withheld)
        .await?
        .is_none());

    // Denied: the intruder resolves only in the co-located identity's
    // directory. Bob's next update reaching the laptop orders the refusal
    // after a window in which the phone demonstrably serves the audience.
    intruder
        .import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::BOB, phone_ticket)
        .await?;
    bob.write(
        ids::BOB,
        ids::BOB,
        author,
        &email,
        b"bob@sentinel.example.org",
    )
    .await?;
    assert!(
        wait_entry_is(
            &a_laptop,
            ids::ALICE,
            ids::BOB,
            &email,
            b"bob@sentinel.example.org"
        )
        .await?,
        "the sentinel update did not reach the audience laptop"
    );
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(intruder
        .read(ids::ALICE_AT_LEISURE, ids::BOB, &email)
        .await?
        .is_none());
    assert!(intruder
        .list(ids::ALICE_AT_LEISURE, ids::BOB, None)
        .await?
        .is_empty());

    // Withdrawal: neither device advances past what it was delivered.
    b_own.withdraw_grant(ids::BOB).await?;
    assert!(
        eventually(|| async {
            Ok(phone_peer
                .read_grant(ids::BOB, ids::ALICE)
                .await?
                .granted()
                .is_none())
        })
        .await?,
        "the withdrawal did not reach the phone"
    );
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@after-withdrawal")
        .await?;
    tokio::time::sleep(RECONCILE * 6).await;
    for (name, node) in [("phone", &a_phone), ("laptop", &a_laptop)] {
        assert!(
            node.read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@sentinel.example.org"),
            "the {name} advanced past its last-granted value after withdrawal"
        );
    }

    a_phone.shutdown().await?;
    a_laptop.shutdown().await?;
    intruder.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}
