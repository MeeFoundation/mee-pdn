//! The connection metadata store at the data layer: dedicated replicas per
//! connection direction, the own→peer flip, grants riding inside (publish /
//! read / withdraw, payload-waiting), replication across both identities'
//! devices, last-writer-wins convergence — and the access pairs of
//! Invariant 3, each allowed path next to its tightest denial.
//!
//! Establishment (the pairing dialogue) lives in pdn-node; here the tickets
//! travel by direct handover, exactly the store-level acts the dialogue and
//! the directory perform.

use anyhow::Result;
use data_layer::{
    claim_id_of, AddrInfoOptions, ConnectionMetadataStore, DocTicket, ReadGrant, ShareMode,
    SyncNode,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, ids};

/// Create a data namespace for `issuer` on `node` and return its read
/// ticket — a real whole-store ticket for grants to carry.
async fn data_ticket(node: &mut SyncNode, issuer: PdnId) -> Result<DocTicket> {
    node.create_namespace(issuer).await?;
    node.share_ticket(issuer, ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await
}

/// Wait until `store` reads the grant for `issuer` as exactly `expected`.
async fn wait_grant_is(
    store: &ConnectionMetadataStore,
    issuer: PdnId,
    expected: &DocTicket,
) -> Result<bool> {
    let expected = expected.to_string();
    eventually(|| async {
        Ok(store
            .read_grant(issuer)
            .await?
            .is_some_and(|t| t.to_string() == expected))
    })
    .await
}

/// Dedicated replicas: creation yields a fresh replica per direction, the
/// two directions of one connection are distinct, and one identity's
/// connections do not share a replica — a grant toward one counterparty is
/// invisible in the store toward another. The own→peer flip: what the
/// issuer writes into `own`, the counterparty reads from `peer`. Import
/// binds before content: the imported handle is usable at once, reads
/// absent, and converges without any re-import.
#[tokio::test(flavor = "multi_thread")]
async fn dedicated_replicas_own_peer_flip_and_isolation() -> Result<()> {
    let mut alice = SyncNode::spawn().await?;
    let bob = SyncNode::spawn().await?;
    let carol = SyncNode::spawn().await?;

    // Alice issues one store per counterparty; Bob issues one toward Alice.
    let a_own_b = ConnectionMetadataStore::create(&alice).await?;
    let a_own_c = ConnectionMetadataStore::create(&alice).await?;
    let b_own_a = ConnectionMetadataStore::create(&bob).await?;

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
        a_own_b
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    let c_peer_a = ConnectionMetadataStore::import(
        &carol,
        a_own_c
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;

    // Import binds before content arrives: the handle is usable at once and
    // reads return absent — nothing has been published yet.
    assert!(b_peer_a.read_grant(ids::ALICE).await?.is_none());
    assert!(b_peer_a.list_grants().await?.is_empty());

    // Alice grants her data store toward Bob and a second one toward Carol.
    let ticket_for_bob = data_ticket(&mut alice, ids::ALICE).await?;
    let ticket_for_carol = data_ticket(&mut alice, ids::ALICE_AT_WORK).await?;
    a_own_b.publish_grant(ids::ALICE, &ticket_for_bob).await?;
    a_own_c
        .publish_grant(ids::ALICE_AT_WORK, &ticket_for_carol)
        .await?;

    // The own→peer flip: the entry written into `own` is read from the
    // counterpart's `peer` — the same replica at both sides, no re-import.
    assert!(
        wait_grant_is(&b_peer_a, ids::ALICE, &ticket_for_bob).await?,
        "grant did not converge from Alice's own store to Bob's peer store"
    );
    assert!(
        wait_grant_is(&c_peer_a, ids::ALICE_AT_WORK, &ticket_for_carol).await?,
        "grant did not converge from Alice's own store to Carol's peer store"
    );

    // Per-connection isolation, probed after both replicas demonstrably
    // converged: the grant toward Bob never appears in Carol's store, and
    // vice versa — and nothing of Alice's stores leaked into Bob's own
    // reverse-direction replica.
    assert_eq!(c_peer_a.list_grants().await?, vec![ids::ALICE_AT_WORK]);
    assert!(c_peer_a.read_grant(ids::ALICE).await?.is_none());
    assert_eq!(b_peer_a.list_grants().await?, vec![ids::ALICE]);
    assert!(b_own_a.list_grants().await?.is_empty());

    alice.shutdown().await?;
    bob.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}

/// Grants over the pair's lifetime, across both identities' devices: a
/// round-trip, a grant published long after the exchange with no new
/// tickets handed over, a withdrawal that reads as absent everywhere, and
/// concurrent updates of one grant key converging to a single entry on
/// every device. Reads are payload-waiting throughout: a grant lists as
/// soon as its record syncs and reads absent until its payload arrives —
/// the polls below ride exactly that contract.
#[tokio::test(flavor = "multi_thread")]
async fn grants_replicate_withdraw_and_converge_across_devices() -> Result<()> {
    let mut a_phone = SyncNode::spawn().await?;
    let mut a_laptop = SyncNode::spawn().await?;
    let b_phone = SyncNode::spawn().await?;
    let b_laptop = SyncNode::spawn().await?;

    // Alice's phone issues the store toward Bob. Her laptop opens it from
    // the write ticket (the directory's own-kind path); Bob's devices open
    // it from the read ticket (the establishment / peer-kind path).
    let own_phone = ConnectionMetadataStore::create(&a_phone).await?;
    let write_ticket = own_phone
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let read_ticket = own_phone
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let own_laptop = ConnectionMetadataStore::import(&a_laptop, write_ticket).await?;
    let peer_b_phone = ConnectionMetadataStore::import(&b_phone, read_ticket.clone()).await?;
    let peer_b_laptop = ConnectionMetadataStore::import(&b_laptop, read_ticket).await?;

    // Written on the issuer's phone, read on the issuer's laptop and on
    // both of the counterparty's devices.
    let first = data_ticket(&mut a_phone, ids::ALICE).await?;
    own_phone.publish_grant(ids::ALICE, &first).await?;
    for (name, store) in [
        ("issuer's laptop", &own_laptop),
        ("counterparty's phone", &peer_b_phone),
        ("counterparty's laptop", &peer_b_laptop),
    ] {
        assert!(
            wait_grant_is(store, ids::ALICE, &first).await?,
            "grant did not converge to the {name}"
        );
    }

    // A grant published after the exchange crosses with no new tickets
    // handed over — the channel outlives the pairing moment.
    let second = data_ticket(&mut a_phone, ids::ALICE_AT_WORK).await?;
    own_phone.publish_grant(ids::ALICE_AT_WORK, &second).await?;
    assert!(
        wait_grant_is(&peer_b_phone, ids::ALICE_AT_WORK, &second).await?,
        "a later grant did not reach the counterparty over the existing pair"
    );

    // Withdrawal: the tombstone replicates and the grant reads as absent —
    // and no longer lists — on the counterparty.
    own_phone.withdraw_grant(ids::ALICE).await?;
    assert!(
        eventually(|| async { Ok(peer_b_phone.read_grant(ids::ALICE).await?.is_none()) }).await?,
        "withdrawn grant still reads on the counterparty"
    );
    assert!(
        eventually(|| async { Ok(!peer_b_phone.list_grants().await?.contains(&ids::ALICE)) })
            .await?,
        "withdrawn grant still lists on the counterparty"
    );

    // Concurrent updates of one grant key from the issuer's two devices:
    // every device of both identities resolves to the same single entry.
    let from_phone = data_ticket(&mut a_phone, ids::ALICE_AT_LEISURE).await?;
    let from_laptop = data_ticket(&mut a_laptop, ids::ALICE_AT_LEISURE).await?;
    own_phone
        .publish_grant(ids::ALICE_AT_LEISURE, &from_phone)
        .await?;
    own_laptop
        .publish_grant(ids::ALICE_AT_LEISURE, &from_laptop)
        .await?;
    let stores = [&own_phone, &own_laptop, &peer_b_phone, &peer_b_laptop];
    assert!(
        eventually(|| async {
            let mut seen = Vec::new();
            for store in stores {
                match store.read_grant(ids::ALICE_AT_LEISURE).await? {
                    Some(ticket) => seen.push(ticket.to_string()),
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

/// One record per issuer, its width explicit.
///
/// Widening: a scoped grant then a whole-store grant to the same issuer —
/// the counterparty's scoped read goes absent and the whole-store read
/// takes effect, with no stale capability left to mask the widening.
/// Narrowing back: a scoped publish replaces the whole-store record the
/// same way. Withdrawal: one act removes the grant whatever its width, and
/// at no point does either side read a grant wider than the last published
/// record — a half-withdrawn state is unrepresentable.
#[tokio::test(flavor = "multi_thread")]
async fn one_grant_record_widens_narrows_and_withdraws_atomically() -> Result<()> {
    let mut alice = SyncNode::spawn().await?;
    let bob = SyncNode::spawn().await?;

    // Alice's own store toward Bob; Bob imports the read ticket.
    let own = ConnectionMetadataStore::create(&alice).await?;
    let b_peer = ConnectionMetadataStore::import(
        &bob,
        own.share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;

    let ticket = data_ticket(&mut alice, ids::ALICE).await?;
    let email = EntryPath::new("contact/email")?;
    let grant = ReadGrant {
        issuer: ids::ALICE,
        audience: ids::BOB,
        claims: NonEmpty::new(claim_id_of(&ids::ALICE, &email)),
        write: false,
    };

    // Scoped first: the counterparty reads the capability, and the
    // whole-store read is absent — one record, one width at a time.
    own.publish_scoped_grant(&grant, &ticket).await?;
    assert!(
        eventually(|| async { Ok(b_peer.read_scoped_grant(ids::ALICE).await?.is_some()) }).await?,
        "the scoped grant did not converge to the counterparty"
    );
    assert!(b_peer.read_grant(ids::ALICE).await?.is_none());

    // Widen: whole-store replaces the scoped record wholesale. The scoped
    // read goes absent — no stale capability survives to mask the widening.
    own.publish_grant(ids::ALICE, &ticket).await?;
    assert!(
        eventually(|| async { Ok(b_peer.read_grant(ids::ALICE).await?.is_some()) }).await?,
        "the widened grant did not take effect on the counterparty"
    );
    assert!(b_peer.read_scoped_grant(ids::ALICE).await?.is_none());

    // Narrow back: the scoped publish replaces the whole-store record the
    // same way, in one write.
    own.publish_scoped_grant(&grant, &ticket).await?;
    assert!(
        eventually(|| async { Ok(b_peer.read_scoped_grant(ids::ALICE).await?.is_some()) }).await?,
        "the narrowed grant did not take effect on the counterparty"
    );
    assert!(b_peer.read_grant(ids::ALICE).await?.is_none());

    // Withdraw: one tombstone removes the grant whatever its width — both
    // reads absent, the issuer unlisted, on both sides.
    own.withdraw_grant(ids::ALICE).await?;
    assert!(own.read_scoped_grant(ids::ALICE).await?.is_none());
    assert!(own.read_grant(ids::ALICE).await?.is_none());
    assert!(
        eventually(|| async {
            Ok(b_peer.read_scoped_grant(ids::ALICE).await?.is_none()
                && b_peer.read_grant(ids::ALICE).await?.is_none()
                && !b_peer.list_grants().await?.contains(&ids::ALICE))
        })
        .await?,
        "the withdrawal did not converge to the counterparty"
    );

    alice.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}

/// A device-shared replica refuses a data import: a connection metadata
/// store — like a directory — is tracked but not data-bound, and a ticket
/// naming its namespace must not repurpose it as a data namespace. Honoring
/// it would overwrite the store's tracking (strategy and contacts) on the
/// word of whoever minted the ticket, and the grantee downgrade would cut
/// the store's live path by leaving the gossip swarm.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_shared_replica_refuses_a_data_import() -> Result<()> {
    let alice = SyncNode::spawn().await?;
    let own = ConnectionMetadataStore::create(&alice).await?;
    let ticket = own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;

    assert!(
        alice
            .import_namespace_scoped(ids::BOB, ticket.clone())
            .await
            .is_err(),
        "a scoped data import must refuse a device-shared replica's namespace"
    );
    assert!(
        alice.import_namespace(ids::BOB, ticket).await.is_err(),
        "a device data import must refuse a device-shared replica's namespace"
    );
    // The store is untouched: still writable through its own surface.
    own.publish_device(alice.node_id()).await?;
    assert_eq!(own.published_devices().await?, vec![alice.node_id()]);

    alice.shutdown().await?;
    Ok(())
}

/// Device records assert once. Opening machinery uses the ensure form: a
/// first touch publishes, a live record is left untouched, and a
/// *withdrawn* record is not resurrected — an unconditional publish would
/// out-bid the tombstone by wall clock on every pair opening. Deliberate
/// re-assertion stays a distinct act (`publish_device`). The tombstone is
/// an agreement honest devices keep; this test pins that they keep it by
/// default.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_device_record_is_not_resurrected_by_pair_opening() -> Result<()> {
    let alice = SyncNode::spawn().await?;
    let own = ConnectionMetadataStore::create(&alice).await?;
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

/// The access pairs of Invariant 3, each allowed path with its tightest
/// denial. Write: the issuer's second device writes via the directory's
/// write ticket ⟷ the counterparty, holding only the read ticket, cannot
/// write and creates no entry. Read: the counterparty reads the whole store
/// ⟷ a third identity — itself sharing a metadata pair with the issuer —
/// holds no replica of this pair, no ticket to it, and reads nothing that
/// reveals its existence.
#[tokio::test(flavor = "multi_thread")]
async fn issuer_devices_write_counterparty_reads_third_party_observes_nothing() -> Result<()> {
    let mut a_phone = SyncNode::spawn().await?;
    let mut a_laptop = SyncNode::spawn().await?;
    let mut bob = SyncNode::spawn().await?;
    let carol = SyncNode::spawn().await?;

    // The A→B pair: laptop on the write ticket, Bob on the read ticket.
    let own_b_phone = ConnectionMetadataStore::create(&a_phone).await?;
    let own_b_laptop = ConnectionMetadataStore::import(
        &a_laptop,
        own_b_phone
            .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    let b_peer = ConnectionMetadataStore::import(
        &bob,
        own_b_phone
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;

    // Carol shares state with Alice too — her own pair, a distinct replica.
    let own_toward_carol = ConnectionMetadataStore::create(&a_phone).await?;
    let c_peer = ConnectionMetadataStore::import(
        &carol,
        own_toward_carol
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;

    // Allowed: the issuer's second device writes a grant through the write
    // ticket, and the counterparty reads it — the whole store is its
    // audience.
    let from_laptop = data_ticket(&mut a_laptop, ids::ALICE_AT_WORK).await?;
    own_b_laptop
        .publish_grant(ids::ALICE_AT_WORK, &from_laptop)
        .await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE_AT_WORK, &from_laptop).await?,
        "the issuer's second device's write did not reach the counterparty"
    );

    // Denied: the counterparty holds only the read ticket — its write is
    // refused outright.
    let bob_ticket = data_ticket(&mut bob, ids::BOB).await?;
    assert!(
        b_peer.publish_grant(ids::BOB, &bob_ticket).await.is_err(),
        "a write through a read-only ticket must be refused"
    );

    // ...and created no entry: a later legitimate write converges — so
    // replication demonstrably flowed after the refusal — while the refused
    // key reads absent at the issuer and at the counterparty itself.
    let sentinel = data_ticket(&mut a_phone, ids::ALICE).await?;
    own_b_phone.publish_grant(ids::ALICE, &sentinel).await?;
    assert!(
        wait_grant_is(&b_peer, ids::ALICE, &sentinel).await?,
        "the sentinel grant did not converge after the refused write"
    );
    assert!(own_b_phone.read_grant(ids::BOB).await?.is_none());
    assert!(b_peer.read_grant(ids::BOB).await?.is_none());

    // Denied: Carol — connected to Alice herself — observes nothing of the
    // A→B store. Her pair is a different replica, she was handed no ticket
    // to the A→B one, and nothing she can read mentions it: her store
    // carries exactly what Alice granted her, none of Bob's grants.
    let for_carol = data_ticket(&mut a_phone, ids::ALICE_AT_LEISURE).await?;
    own_toward_carol
        .publish_grant(ids::ALICE_AT_LEISURE, &for_carol)
        .await?;
    assert!(
        wait_grant_is(&c_peer, ids::ALICE_AT_LEISURE, &for_carol).await?,
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
    assert!(c_peer.read_grant(ids::ALICE).await?.is_none());
    assert!(c_peer.read_grant(ids::ALICE_AT_WORK).await?.is_none());

    a_phone.shutdown().await?;
    a_laptop.shutdown().await?;
    bob.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}
