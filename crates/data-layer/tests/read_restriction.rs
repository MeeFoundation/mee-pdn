//! Capability-filtered reconciliation — Invariant 2 at the data layer.
//!
//! An issuer grants a counterparty read on a subset of its claims; the
//! counterparty receives exactly that subset over reconciliation, and the
//! withheld entries never arrive — content or existence. Per
//! `code-practices/access-control-tests.md`, every allowed path sits next
//! to its tightest denial: the outsider, the identity of the replica's
//! ticket without a grant, and (for writes) the read-only grant identity.
//!
//! Establishment (the pairing dialogue) and device-set publication live in
//! pdn-node; here the tickets and records travel by direct handover,
//! exactly the store-level acts the ceremonies perform.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    claim_id_of, identity_of, AddrInfoOptions, ConnectionMetadataStore, Contact, GrantRead,
    GrantedClaim, PrivateMetadataStore, ReadGrant, ShareMode, SpawnOptions, SyncNode,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, host_identity, ids, join_identity, wait_devices};

/// The three claims Bob's data store carries in these scenarios.
const GRANTED: &str = "contact/email";
const WITHHELD_A: &str = "contact/phone";
const WITHHELD_B: &str = "notes/diary";

/// Scoped readers have no gossip path, so every negative assertion is "the
/// reader retried over several intervals and was refused" — milliseconds
/// at this cadence instead of the production default's tens of seconds.
const RECONCILE: Duration = Duration::from_millis(500);

async fn spawn_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// A one-claim grant of `issuer`'s entry at `path` toward `audience` —
/// read always, write when `write`.
fn granted(issuer: PdnId, audience: PdnId, path: &EntryPath, write: bool) -> ReadGrant {
    ReadGrant {
        issuer,
        audience,
        claims: NonEmpty::new(GrantedClaim {
            claim: claim_id_of(&issuer, path),
            write,
        }),
    }
}

/// The serving side: Bob's node hosting his identity, plus a connection
/// toward `peer` registered for caller classification.
struct ServingSide {
    own_toward_peer: ConnectionMetadataStore,
    own_read_ticket: data_layer::DocTicket,
    /// The device set the access book probes.
    directory: PrivateMetadataStore,
    /// Bob's copy of the peer's own store — the only record he holds of
    /// the peer's devices, and the one a scenario waits on before it
    /// expects a session of that peer to be admitted.
    peer_store: ConnectionMetadataStore,
}

async fn serving_side(
    bob: &SyncNode,
    peer: PdnId,
    peer_own: &ConnectionMetadataStore,
) -> Result<ServingSide> {
    let directory = host_identity(bob, ids::BOB).await?;

    // The connection pair as establishment leaves it.
    let own_toward_peer = ConnectionMetadataStore::create(bob, ids::BOB).await?;
    own_toward_peer.publish_device(bob.node_id()).await?;
    let own_read_ticket = own_toward_peer
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let peer_store = ConnectionMetadataStore::import(
        bob,
        ids::BOB,
        peer_own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    bob.host_connection(ids::BOB, peer, &own_toward_peer, &peer_store)?;

    Ok(ServingSide {
        own_toward_peer,
        own_read_ticket,
        directory,
        peer_store,
    })
}

/// Write Bob's three entries into his data namespace.
async fn write_bobs_entries(bob: &SyncNode) -> Result<()> {
    let author = bob.default_author(ids::BOB)?;
    for (path, payload) in [
        (GRANTED, b"bob@example.org".as_slice()),
        (WITHHELD_A, b"+1-555-0100".as_slice()),
        (WITHHELD_B, b"dear diary".as_slice()),
    ] {
        bob.write(ids::BOB, ids::BOB, author, &EntryPath::new(path)?, payload)
            .await?;
    }
    Ok(())
}

/// Invariant 2. Allowed: Alice, granted read on exactly `contact/email`,
/// receives it and its updates. Denied: the withheld entries never reach
/// her, not even after a proven second wave (existence hidden); Carol, with
/// the leaked ticket and no grant, obtains nothing; Alice's read ticket
/// carries no namespace secret, so her local write fails outright.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn read_restricted_peer_receives_exactly_the_granted_subset() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let carol = spawn_node().await?;
    let _carol_dir = host_identity(&carol, ids::CAROL).await?;

    // Alice's reverse-direction store, carrying her published device set.
    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;

    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    // Bob's data namespace with the three entries.
    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // Read-only, so the grant ships a read ticket.
    let email = EntryPath::new(GRANTED)?;
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &data_read_ticket)
        .await?;

    // Alice consumes the grant as the grant binder would.
    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (received_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    assert_eq!(received_grant.claims, grant.claims);
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Allowed: the granted entry arrives, with its payload.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the granted peer"
    );

    // Denied (read-only cannot write).
    let alice_author = alice.default_author(ids::ALICE)?;
    assert!(
        alice
            .write(
                ids::ALICE,
                ids::BOB,
                alice_author,
                &email,
                b"overwrite attempt"
            )
            .await
            .is_err(),
        "a write through a read-only grant must be refused"
    );

    // Denied (ticket without a grant): Carol imports the leaked read ticket.
    carol
        .import_namespace_scoped(ids::CAROL, ids::BOB, data_read_ticket)
        .await?;

    // Sentinel: a second wave proven end to end orders the absence assertions
    // below.
    let author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@new.example.org")
        .await?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@new.example.org"))
        })
        .await?,
        "the sentinel update did not reach the granted peer"
    );

    // Denied (existence hidden): after the proven second wave, Alice's view
    // lists exactly the granted entry.
    let listed: Vec<String> = alice
        .list(ids::ALICE, ids::BOB, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec![GRANTED.to_owned()],
        "the granted peer's view must contain exactly the granted subset"
    );
    for withheld in [WITHHELD_A, WITHHELD_B] {
        assert!(
            alice
                .read(ids::ALICE, ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a withheld entry leaked to the granted peer: {withheld}"
        );
    }

    // ...and Carol, with the ticket but no grant, has obtained nothing:
    // three more of her intervals after the proven second wave make this
    // "she tried repeatedly and was refused", not a poll that outran her
    // first dial.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        carol.list(ids::CAROL, ids::BOB, None).await?.is_empty(),
        "a ticket identity without a grant must obtain nothing"
    );
    assert!(carol.read(ids::CAROL, ids::BOB, &email).await?.is_none());

    bob.shutdown().await?;
    alice.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}

/// A device set never widens a session that names another identity:
/// Alice, a scoped grantee of Bob who is also a device in Bob's own
/// directory, receives exactly her granted claim — pending or confirmed
/// alike, because the session names ALICE and the rights follow the
/// named identity alone (ADR-0013).
///
/// Denied: the withheld entries stay absent through both, ordered after
/// a granted read that proves the path live.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, pending and confirmed in one place
async fn a_pending_device_registration_confers_nothing() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The registration a linking dialogue leaves on the inviter.
    serving
        .directory
        .add_pending_device(alice.node_id())
        .await?;

    // Alice's grant on one claim, consumed as the grant binder would.
    let email = EntryPath::new(GRANTED)?;
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &data_read_ticket)
        .await?;
    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Allowed, by the grant alone: the granted entry arrives, which also
    // proves the reconciliation path is live for the denial below.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the granted peer"
    );

    // Denied: pending is not membership, waited out over three more of
    // Alice's intervals after the path proved live.
    tokio::time::sleep(RECONCILE * 3).await;
    for withheld in [WITHHELD_A, WITHHELD_B] {
        assert!(
            alice
                .read(ids::ALICE, ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a pending registration served a withheld entry: {withheld}"
        );
    }

    // The confirmation: Alice's node is a device in Bob's own directory
    // now. It widens nothing — her session names ALICE, and what ALICE
    // was granted is one claim. Sentinel: a fresh write on the granted
    // claim crossing proves the path is live through the window.
    serving.directory.confirm_device(alice.node_id()).await?;
    let bob_author = bob.default_author(ids::BOB)?;
    bob.write(
        ids::BOB,
        ids::BOB,
        bob_author,
        &email,
        b"bob@confirmed.example.org",
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@confirmed.example.org"))
        })
        .await?,
        "the granted claim stopped arriving after the confirmation"
    );
    for withheld in [WITHHELD_A, WITHHELD_B] {
        assert!(
            alice
                .read(ids::ALICE, ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a device of the issuer's own directory widened a session naming another identity: \
             {withheld}"
        );
    }

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// A grant in Bob's store toward Alice whose capability names Carol serves
/// Alice nothing: the capability's audience authorizes, not the record's
/// position — the one guard between "a device the connection is toward" and
/// "a device the grant is addressed to".
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_addressed_to_another_identity_serves_nobody() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // Published into Bob's store toward Alice — but the capability names
    // Carol, not Alice, as its audience.
    let email = EntryPath::new(GRANTED)?;
    let misaddressed = granted(ids::BOB, ids::CAROL, &email, false);
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&misaddressed, &data_read_ticket)
        .await?;

    // Alice opens the pair and imports the namespace exactly as a rightful
    // grantee would — the ticket is addressing, so nothing here fails.
    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    // The record converges to Alice — proof the nodes replicate, so the
    // data denial below is "refused", not "not yet connected" — and reads
    // as a decided absence.
    assert!(
        eventually(|| async {
            Ok(matches!(
                alice_peer.read_grant(ids::BOB, ids::ALICE).await?,
                GrantRead::None
            ))
        })
        .await?,
        "the misaddressed grant must read as a decided absence, not as a grant"
    );
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, data_read_ticket)
        .await?;

    // Bob updates the claim; Alice re-dials Bob every reconcile interval,
    // and after three past a fresh write "she asked repeatedly and was
    // refused" is what holds this green.
    let author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@new.example.org")
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        alice.list(ids::ALICE, ids::BOB, None).await?.is_empty(),
        "a grant naming another identity's audience must serve nothing"
    );
    assert!(alice.read(ids::ALICE, ids::BOB, &email).await?.is_none());

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// A grant carrying write ships a write ticket; the ingest gate (ADR-0008)
/// bounds the secret to the granted claim. Allowed: Alice writes
/// `shared/note` and Bob converges. Denied: Bob's other entries still never
/// reach her, and an entry she signs at an ungranted claim is refused by
/// the gate — Bob's own survives — each ordered by a granted-claim
/// round-trip.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn write_grant_round_trips_while_reads_stay_scoped() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The shared, writable claim.
    let note = EntryPath::new("shared/note")?;
    let bob_author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, bob_author, &note, b"from bob")
        .await?;

    // The write ticket carries the namespace secret; the ingest gate is what
    // scopes it.
    let grant = granted(ids::BOB, ids::ALICE, &note, true);
    let data_write_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &data_write_ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // The granted entry arrives at Alice first (so her write below is an
    // update, not a blind create).
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from bob"))
        })
        .await?,
        "the granted entry did not reach the write-granted peer"
    );

    // Allowed: Alice writes the granted claim under her own author, and
    // Bob converges on her value.
    let alice_author = alice.default_author(ids::ALICE)?;
    alice
        .write(ids::ALICE, ids::BOB, alice_author, &note, b"from alice")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from alice"))
        })
        .await?,
        "the write-granted peer's write did not reach the issuer"
    );

    // Denied: bidirectional replication demonstrably ran, yet the ungranted
    // entries never reached Alice.
    let listed: Vec<String> = alice
        .list(ids::ALICE, ids::BOB, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["shared/note".to_owned()],
        "a write grant must not widen the read scope"
    );

    // Denied (write outside the write set): Bob's own entry survives; Alice's
    // replica keeps her provisional write.
    let diary = EntryPath::new(WITHHELD_B)?;
    alice
        .write(
            ids::ALICE,
            ids::BOB,
            alice_author,
            &diary,
            b"ungranted overwrite",
        )
        .await?;
    // Sentinel: the sessions that carried and refused the ungranted write
    // have run.
    alice
        .write(
            ids::ALICE,
            ids::BOB,
            alice_author,
            &note,
            b"from alice again",
        )
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from alice again"))
        })
        .await?,
        "the sentinel granted-claim write did not round-trip"
    );
    assert!(
        bob.read(ids::BOB, ids::BOB, &diary)
            .await?
            .is_some_and(|p| p == b"dear diary"),
        "an ungranted write leaked through the ingest gate"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// A withdrawn grant refuses the next session while what was delivered
/// stays readable (Invariant 2 governs acquisition, not retention).
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_grant_refuses_the_next_session_but_keeps_delivered_data() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    let email = EntryPath::new(GRANTED)?;
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Allowed: the granted entry converges before the withdrawal.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the granted peer before withdrawal"
    );

    // One tombstone over the one record; the issuer's own book reads it as
    // absent at once.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    assert!(serving
        .own_toward_peer
        .read_grant(ids::BOB, ids::ALICE)
        .await?
        .granted()
        .is_none());

    // Rights are frozen per session: one interval drains any in-flight
    // pre-withdrawal session before the update exists.
    tokio::time::sleep(RECONCILE).await;

    // Denied: an update written after the withdrawal never arrives.
    let author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, author, &email, b"bob@after-withdrawal")
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        alice
            .read(ids::ALICE, ids::BOB, &email)
            .await?
            .is_some_and(|p| p == b"bob@example.org"),
        "an update leaked through a withdrawn grant, or delivered data was lost"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// Swarm membership does not bypass the access book: the swarm is
/// content-free, so a member is served what the book grants it per session.
/// Dave joins Bob's swarm with a device-style import and stays a member
/// throughout. Positive control: while granted, he converges on Bob's
/// write. Negative: once the grant is withdrawn, a later write never
/// reaches him.
#[tokio::test(flavor = "multi_thread")]
async fn swarm_membership_does_not_bypass_the_access_book() -> Result<()> {
    /// Absolute: gossip latency does not scale with the reconcile interval.
    const SWARM_WINDOW: Duration = Duration::from_secs(15);

    let bob = spawn_node().await?;
    let dave = spawn_node().await?;
    let _dave_dir = host_identity(&dave, ids::DAVE).await?;

    // Bob's serving side, armed, with a connection toward Dave — so Bob can
    // resolve Dave's node id and carry a grant for him.
    let dave_own = ConnectionMetadataStore::create(&dave, ids::DAVE).await?;
    dave_own.publish_device(dave.node_id()).await?;
    let serving = serving_side(&bob, ids::DAVE, &dave_own).await?;
    // Dave's own half of the pair, registered: without it his replica of
    // it is served to nobody and Bob never reads his device record.
    let dave_peer =
        ConnectionMetadataStore::import(&dave, ids::DAVE, serving.own_read_ticket.clone()).await?;
    dave.host_connection(ids::DAVE, ids::BOB, &dave_own, &dave_peer)?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    let email = EntryPath::new(GRANTED)?;
    let bob_author = bob.default_author(ids::BOB)?;
    let ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;

    // Dave joins Bob's swarm with a device-style import; nothing below removes
    // him.
    dave.import_namespace(ids::DAVE, ids::BOB, ticket).await?;

    // Bob's book carries a grant for Dave on the granted claim.
    let grant_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    let grant = granted(ids::BOB, ids::DAVE, &email, false);
    serving
        .own_toward_peer
        .publish_grant(&grant, &grant_ticket)
        .await?;

    // Bob re-writes each poll so a first announce lands.
    assert!(
        eventually(|| async {
            bob.write(ids::BOB, ids::BOB, bob_author, &email, b"bob@example.org")
                .await?;
            Ok(dave.read(ids::DAVE, ids::BOB, &email).await?.is_some())
        })
        .await?,
        "the granted swarm member did not converge — mesh/positive control failed"
    );

    // Bob withdraws the grant; his own book reads it as absent at once.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    // Drain any pre-withdrawal session before the probe write exists.
    tokio::time::sleep(RECONCILE).await;

    // Negative: a write made after the withdrawal never reaches Dave.
    let after = EntryPath::new(WITHHELD_A)?;
    bob.write(ids::BOB, ids::BOB, bob_author, &after, b"post-withdrawal")
        .await?;
    tokio::time::sleep(SWARM_WINDOW).await;
    assert!(
        dave.read(ids::DAVE, ids::BOB, &after).await?.is_none(),
        "a swarm member received a write after its grant was withdrawn — the swarm carried content"
    );
    // Retained: the negative above is not a wiped replica.
    assert!(dave.read(ids::DAVE, ids::BOB, &email).await?.is_some());

    bob.shutdown().await?;
    dave.shutdown().await?;
    Ok(())
}

/// Mixed rights in one grant. Allowed: the write-granted claim round-trips.
/// Denied: the same identity's write at the read-only claim, signed with the
/// very secret the write ticket carries, never reaches the issuer.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn a_mixed_grant_admits_exactly_its_write_claims() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // One record, a write ticket.
    let email = EntryPath::new(GRANTED)?;
    let phone = EntryPath::new(WITHHELD_A)?;
    let mut grant = granted(ids::BOB, ids::ALICE, &email, false);
    grant.claims.push(GrantedClaim {
        claim: claim_id_of(&ids::BOB, &phone),
        write: true,
    });
    let write_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &write_ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Both granted claims arrive.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org")
                && alice
                    .read(ids::ALICE, ids::BOB, &phone)
                    .await?
                    .is_some_and(|p| p == b"+1-555-0100"))
        })
        .await?,
        "the granted claims did not reach the mixed-grant audience"
    );

    // Allowed: the write-granted claim round-trips.
    let alice_author = alice.default_author(ids::ALICE)?;
    alice
        .write(ids::ALICE, ids::BOB, alice_author, &phone, b"+7-999-0001")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, ids::BOB, &phone)
                .await?
                .is_some_and(|p| p == b"+7-999-0001"))
        })
        .await?,
        "the write-granted claim did not round-trip"
    );

    // Denied: the read-only claim, forced with the ticket's secret; the
    // sentinel wave orders the assertion.
    alice
        .write(ids::ALICE, ids::BOB, alice_author, &email, b"forged@alice")
        .await?;
    alice
        .write(ids::ALICE, ids::BOB, alice_author, &phone, b"+7-999-0002")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, ids::BOB, &phone)
                .await?
                .is_some_and(|p| p == b"+7-999-0002"))
        })
        .await?,
        "the sentinel write on the writable claim did not round-trip"
    );
    assert!(
        bob.read(ids::BOB, ids::BOB, &email)
            .await?
            .is_some_and(|p| p == b"bob@example.org"),
        "a write at a read-only claim leaked through the ingest gate"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// Withdrawal closes the write side from the next session; the value
/// accepted before stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_write_grant_refuses_the_next_sessions_writes() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;

    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    let note = EntryPath::new("shared/note")?;
    let bob_author = bob.default_author(ids::BOB)?;
    bob.write(ids::BOB, ids::BOB, bob_author, &note, b"from bob")
        .await?;

    let grant = granted(ids::BOB, ids::ALICE, &note, true);
    let write_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &write_ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Accepted while granted.
    let alice_author = alice.default_author(ids::ALICE)?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from bob"))
        })
        .await?,
        "the granted claim did not arrive before the withdrawal"
    );
    alice
        .write(ids::ALICE, ids::BOB, alice_author, &note, b"from alice")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from alice"))
        })
        .await?,
        "the pre-withdrawal write was not accepted"
    );

    // One interval drains any in-flight pre-withdrawal session.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    tokio::time::sleep(RECONCILE).await;

    // A write made after the withdrawal never reaches the issuer.
    alice
        .write(
            ids::ALICE,
            ids::BOB,
            alice_author,
            &note,
            b"post-withdrawal",
        )
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    let at_bob = bob.read(ids::BOB, ids::BOB, &note).await?;
    assert!(
        at_bob.as_deref() == Some(b"from alice".as_ref()),
        "a write leaked through a withdrawn grant, or the accepted value was lost: {:?}",
        at_bob.as_deref().map(String::from_utf8_lossy)
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// A device of the audience identity that the issuer has not yet seen
/// published obtains nothing, and is served as soon as its own record
/// reaches the issuer — which it carries there itself, over the
/// connection metadata store, whose bound is its ticket rather than the
/// device set it carries (Invariants 1 and 3).
///
/// Denied: over several of the issuer's passes the unpublished device
/// obtains neither the entry nor its existence, while a sibling the
/// issuer already knows keeps converging — so the refusal is the record's
/// doing and not a path that was never live.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn a_device_the_issuer_has_not_seen_published_is_served_once_it_publishes() -> Result<()> {
    let bob = spawn_node().await?;
    let laptop = spawn_node().await?;
    let phone = spawn_node().await?;
    let _laptop_dir = host_identity(&laptop, ids::ALICE).await?;
    let _phone_dir = host_identity(&phone, ids::ALICE).await?;

    // Alice's half of the pair, published from the laptop alone.
    let alice_own = ConnectionMetadataStore::create(&laptop, ids::ALICE).await?;
    alice_own.publish_device(laptop.node_id()).await?;
    let alice_own_write = alice_own
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;
    let email = EntryPath::new(GRANTED)?;
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &data_read_ticket)
        .await?;

    // The laptop consumes the grant as the binder would.
    let laptop_peer =
        ConnectionMetadataStore::import(&laptop, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    laptop.host_connection(ids::ALICE, ids::BOB, &alice_own, &laptop_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&laptop_peer, ids::BOB, ids::ALICE).await?;
    laptop
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket.clone())
        .await?;
    assert!(
        eventually(|| async {
            Ok(laptop
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the device the issuer knows"
    );

    // The phone joins Alice and reaches for the same granted namespace
    // before it has published itself: the issuer's copy of Alice's device
    // set names the laptop alone.
    phone
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;

    // Sentinel: a fresh write reaching the laptop proves the issuer is
    // serving throughout the window the phone spends refused.
    let bob_author = bob.default_author(ids::BOB)?;
    bob.write(
        ids::BOB,
        ids::BOB,
        bob_author,
        &email,
        b"bob@new.example.org",
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(laptop
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@new.example.org"))
        })
        .await?,
        "the sentinel write did not reach the device the issuer knows"
    );

    // Denied: the phone obtained neither the entry nor its existence.
    tokio::time::sleep(RECONCILE * 3).await;
    assert_eq!(
        phone.read(ids::ALICE, ids::BOB, &email).await?,
        None,
        "an unpublished device was served the granted entry"
    );
    assert!(
        phone.list(ids::ALICE, ids::BOB, None).await?.is_empty(),
        "an unpublished device was told the granted entry exists"
    );

    // The phone opens the pair the way a device that linked into Alice
    // does — the write ticket of her own half is in her directory — and
    // publishes itself. Its contacts name the issuer, as the runtime's
    // connection armer sets them, so the record travels without the
    // laptop.
    let phone_own = ConnectionMetadataStore::import(&phone, ids::ALICE, alice_own_write).await?;
    let phone_peer =
        ConnectionMetadataStore::import(&phone, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    phone.host_connection(ids::ALICE, ids::BOB, &phone_own, &phone_peer)?;
    phone.set_doc_contacts(
        ids::ALICE,
        phone_own.namespace(),
        vec![Contact::new(
            bob.dial_handle().addr(),
            identity_of(ids::BOB),
        )],
    )?;
    phone_own.publish_device(phone.node_id()).await?;

    // Allowed: the record reaches the issuer over the ticket-bound store,
    // and the next session serves the phone exactly the granted claim.
    assert!(
        eventually(|| async {
            Ok(phone
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@new.example.org"))
        })
        .await?,
        "the device was not served once its record reached the issuer"
    );
    for withheld in [WITHHELD_A, WITHHELD_B] {
        assert!(
            phone
                .read(ids::ALICE, ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a withheld entry leaked to the newly published device: {withheld}"
        );
    }

    bob.shutdown().await?;
    laptop.shutdown().await?;
    phone.shutdown().await?;
    Ok(())
}

/// Grant `audience` read on Bob's entry at `path`, and let that identity
/// consume the grant the way the binder does: the connection pair as
/// establishment leaves it on both nodes, the grant record published on
/// Bob's side, and the ticket it carries imported on Alice's node for
/// that identity alone.
async fn grant_one_claim_to(
    bob: &SyncNode,
    alice: &SyncNode,
    audience: PdnId,
    path: &EntryPath,
) -> Result<()> {
    let alice_own = ConnectionMetadataStore::create(alice, audience).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let alice_read = alice_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let bob_own = ConnectionMetadataStore::create(bob, ids::BOB).await?;
    bob_own.publish_device(bob.node_id()).await?;
    let bob_read = bob_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let alice_as_bob_sees = ConnectionMetadataStore::import(bob, ids::BOB, alice_read).await?;
    bob.host_connection(ids::BOB, audience, &bob_own, &alice_as_bob_sees)?;

    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    bob_own
        .publish_grant(&granted(ids::BOB, audience, path, false), &data_read_ticket)
        .await?;

    let bob_as_alice_sees = ConnectionMetadataStore::import(alice, audience, bob_read).await?;
    alice.host_connection(audience, ids::BOB, &alice_own, &bob_as_alice_sees)?;
    let (_grant, ticket) = eventually_scoped_grant(&bob_as_alice_sees, ids::BOB, audience).await?;
    alice
        .import_namespace_scoped(audience, ids::BOB, ticket)
        .await?;
    Ok(())
}

/// Two audiences of one issuer hosted side by side on one node each
/// receive exactly the claim granted to it: rights follow the identity a
/// session names, and are never the union of what the node's identities
/// hold (ADR-0013). Both audiences dial from one node id, so only the
/// identity the session names tells the two apart.
///
/// Denied: neither audience obtains the other's claim — over the network
/// from the issuer or over the in-process path from its co-located
/// sibling, which holds a replica of the same namespace — nor the claim
/// withheld from both. A second wave on each granted claim proves both
/// sessions live and orders the absences.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, both audiences and both denials in one place
async fn two_co_located_audiences_of_one_issuer_receive_each_its_own_claim() -> Result<()> {
    let bob = spawn_node().await?;
    let _bob_dir = host_identity(&bob, ids::BOB).await?;
    let alice = spawn_node().await?;
    let _work_dir = host_identity(&alice, ids::ALICE_AT_WORK).await?;
    let _leisure_dir = host_identity(&alice, ids::ALICE_AT_LEISURE).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The three constants carry the roles of the first scenario; here two
    // of them are one audience's claim each.
    let work_claim = EntryPath::new(GRANTED)?;
    let leisure_claim = EntryPath::new(WITHHELD_A)?;
    let withheld = EntryPath::new(WITHHELD_B)?;
    grant_one_claim_to(&bob, &alice, ids::ALICE_AT_WORK, &work_claim).await?;
    grant_one_claim_to(&bob, &alice, ids::ALICE_AT_LEISURE, &leisure_claim).await?;

    // Allowed: each audience converges the claim its own grant names.
    for (audience, claim, payload) in [
        (
            ids::ALICE_AT_WORK,
            &work_claim,
            b"bob@example.org".as_slice(),
        ),
        (
            ids::ALICE_AT_LEISURE,
            &leisure_claim,
            b"+1-555-0100".as_slice(),
        ),
    ] {
        assert!(
            eventually(|| async {
                Ok(alice
                    .read(audience, ids::BOB, claim)
                    .await?
                    .is_some_and(|p| p == payload))
            })
            .await?,
            "the granted claim did not reach {audience}"
        );
    }

    // Sentinel: a second value on each granted claim, proven across, puts
    // a further wave of both sessions behind the absences below.
    let bob_author = bob.default_author(ids::BOB)?;
    bob.write(
        ids::BOB,
        ids::BOB,
        bob_author,
        &work_claim,
        b"bob@second.example.org",
    )
    .await?;
    bob.write(
        ids::BOB,
        ids::BOB,
        bob_author,
        &leisure_claim,
        b"+1-555-0200",
    )
    .await?;
    for (audience, claim, payload) in [
        (
            ids::ALICE_AT_WORK,
            &work_claim,
            b"bob@second.example.org".as_slice(),
        ),
        (
            ids::ALICE_AT_LEISURE,
            &leisure_claim,
            b"+1-555-0200".as_slice(),
        ),
    ] {
        assert!(
            eventually(|| async {
                Ok(alice
                    .read(audience, ids::BOB, claim)
                    .await?
                    .is_some_and(|p| p == payload))
            })
            .await?,
            "the second wave did not cross for {audience}"
        );
    }

    // Denied: the other audience's claim, and the one withheld from both
    // — neither the payload nor the existence.
    for (audience, other) in [
        (ids::ALICE_AT_WORK, &leisure_claim),
        (ids::ALICE_AT_LEISURE, &work_claim),
    ] {
        assert_eq!(
            alice.read(audience, ids::BOB, other).await?,
            None,
            "{audience} obtained the claim granted to its co-located sibling"
        );
        assert!(
            alice
                .list(audience, ids::BOB, None)
                .await?
                .iter()
                .all(|entry| &entry.path != other),
            "{audience} learned the existence of its co-located sibling's claim"
        );
        assert_eq!(
            alice.read(audience, ids::BOB, &withheld).await?,
            None,
            "{audience} obtained a claim withheld from both"
        );
    }

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// A data replica is refused a caller its identity's records entitle to
/// nothing, the identity of that replica's own read ticket included, while
/// the identity's directory and its connection metadata store stay bound
/// to their tickets (Invariants 1 and 3).
///
/// Denied: Carol, connected to Bob and granted no claim, obtains neither
/// the entries nor their existence over several of her passes although
/// she holds the data replica's read ticket. Ordered after the two paths
/// that do run on a ticket alone — Bob's laptop replicating his
/// directory, and Carol's import of Bob's connection metadata store — so
/// the refusal is the data replica's verdict and not a node that answers
/// nobody.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_replica_no_record_judges_is_refused_its_own_ticket() -> Result<()> {
    let bob = spawn_node().await?;
    let laptop = spawn_node().await?;
    let carol = spawn_node().await?;
    let _carol_dir = host_identity(&carol, ids::CAROL).await?;

    // Bob and Carol are connected and Bob has granted her nothing: his
    // records place her and entitle her to no claim, the tightest caller
    // the data replica has to refuse.
    let carol_own = ConnectionMetadataStore::create(&carol, ids::CAROL).await?;
    carol_own.publish_device(carol.node_id()).await?;
    let serving = serving_side(&bob, ids::CAROL, &carol_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The data replica's own read ticket, out of band in Carol's hands.
    let data_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    carol
        .import_namespace_scoped(ids::CAROL, ids::BOB, data_ticket)
        .await?;

    // Allowed on its ticket: Bob's laptop joins his device set and
    // replicates his directory.
    let directory_ticket = serving
        .directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = join_identity(&laptop, ids::BOB, directory_ticket).await?;
    serving.directory.add_device(laptop.node_id()).await?;
    assert!(
        wait_devices(&laptop_dir, &[bob.node_id(), laptop.node_id()]).await?,
        "the directory did not replicate to a device of its own identity"
    );

    // Allowed on its ticket: Carol's import of the connection metadata
    // store, whose bound is that ticket and not Bob's grants.
    let bob_as_carol_sees =
        ConnectionMetadataStore::import(&carol, ids::CAROL, serving.own_read_ticket).await?;
    carol.host_connection(ids::CAROL, ids::BOB, &carol_own, &bob_as_carol_sees)?;
    assert!(
        eventually(|| async {
            Ok(bob_as_carol_sees
                .published_devices()
                .await?
                .contains(&bob.node_id()))
        })
        .await?,
        "a connection metadata store stopped being bound by its ticket"
    );

    // Denied: the data replica, over three more of Carol's passes.
    tokio::time::sleep(RECONCILE * 3).await;
    for claim in [GRANTED, WITHHELD_A, WITHHELD_B] {
        assert_eq!(
            carol
                .read(ids::CAROL, ids::BOB, &EntryPath::new(claim)?)
                .await?,
            None,
            "a data replica was served on its ticket alone: {claim}"
        );
    }
    assert!(
        carol.list(ids::CAROL, ids::BOB, None).await?.is_empty(),
        "a data replica leaked the existence of its entries on its ticket alone"
    );

    carol.shutdown().await?;
    laptop.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}

/// A caller is admitted for the identity it names only where that
/// identity's records list the caller's node id: Alice's own node, which
/// her published device set names, is served, and a node that names her
/// identity without being one of her devices is refused as if the
/// replica were not hosted.
///
/// Denied: the impersonating node's session is refused and its replica
/// stays empty over several of its passes, beside the identical session
/// from Alice's own node that succeeds. Both sessions name the identities
/// themselves, through the fixture that lets a caller choose them, the
/// way a forced write produces an entry the gate refuses.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the arrangement and both callers in one place
async fn a_caller_is_admitted_only_where_the_named_identity_lists_its_node() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let _alice_dir = host_identity(&alice, ids::ALICE).await?;
    let mallory = spawn_node().await?;
    let _mallory_dir = host_identity(&mallory, ids::DAVE).await?;

    // Alice's published device set names her node and no other.
    let alice_own = ConnectionMetadataStore::create(&alice, ids::ALICE).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB, ids::BOB).await?;
    write_bobs_entries(&bob).await?;
    let email = EntryPath::new(GRANTED)?;
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(
            &granted(ids::BOB, ids::ALICE, &email, false),
            &data_read_ticket,
        )
        .await?;

    // Both sides register the connection, so Bob's book can judge a
    // session naming Alice at all.
    let alice_peer =
        ConnectionMetadataStore::import(&alice, ids::ALICE, serving.own_read_ticket.clone())
            .await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::ALICE, ids::BOB, received_ticket)
        .await?;
    // Both callers below open their session by hand, so neither keeps a
    // contact: a pass dialing the same pair would meet the hand-made
    // session and be refused as already syncing, whatever the records
    // say.
    alice.set_namespace_contacts(ids::ALICE, ids::BOB, Vec::new())?;

    // Bob's only record of Alice's devices is his copy of her own
    // store; a session opened before it converged would be refused for
    // the record's absence rather than for the caller's.
    assert!(
        eventually(|| async {
            Ok(serving
                .peer_store
                .published_devices()
                .await?
                .contains(&alice.node_id()))
        })
        .await?,
        "Alice's published device never reached Bob"
    );

    // Allowed: Alice's own node, naming her identity, is served — the
    // session itself is the verdict, and the entry follows it once its
    // payload has been fetched.
    let toward_bob = Contact::new(bob.dial_handle().addr(), identity_of(ids::BOB));
    alice
        .sync_as_for_test(
            ids::ALICE,
            ids::BOB,
            toward_bob.clone(),
            identity_of(ids::ALICE),
        )
        .await?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::ALICE, ids::BOB, &email)
                .await?
                .is_some_and(|payload| payload == b"bob@example.org"))
        })
        .await?,
        "a device of the named identity must be served"
    );

    // Denied: a node Alice's records do not name, claiming her identity.
    mallory
        .import_namespace_scoped(ids::DAVE, ids::BOB, data_read_ticket)
        .await?;
    mallory.set_namespace_contacts(ids::DAVE, ids::BOB, Vec::new())?;
    let refused = mallory
        .sync_as_for_test(ids::DAVE, ids::BOB, toward_bob, identity_of(ids::ALICE))
        .await
        .expect_err("a caller the named identity's records do not list must be refused");
    assert!(
        format!("{refused:#}").contains("NotFound"),
        "the refusal must be the not-hosted abort, got: {refused:#}"
    );
    tokio::time::sleep(RECONCILE * 3).await;
    for claim in [GRANTED, WITHHELD_A, WITHHELD_B] {
        assert_eq!(
            mallory
                .read(ids::DAVE, ids::BOB, &EntryPath::new(claim)?)
                .await?,
            None,
            "an impersonating caller obtained an entry: {claim}"
        );
    }

    mallory.shutdown().await?;
    alice.shutdown().await?;
    bob.shutdown().await?;
    Ok(())
}

/// Poll the peer store until the scoped grant for `issuer` is readable
/// (record and payloads arrived), then return it.
async fn eventually_scoped_grant(
    store: &ConnectionMetadataStore,
    issuer: PdnId,
    audience: PdnId,
) -> Result<(ReadGrant, data_layer::DocTicket)> {
    let mut found = None;
    let ok = eventually(|| async {
        Ok(store
            .read_grant(issuer, audience)
            .await?
            .granted()
            .is_some())
    })
    .await?;
    if ok {
        found = store.read_grant(issuer, audience).await?.granted();
    }
    found.ok_or_else(|| anyhow::anyhow!("scoped grant for {issuer} did not arrive"))
}
