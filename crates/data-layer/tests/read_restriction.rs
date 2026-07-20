//! Capability-filtered reconciliation — Invariant 2 at the data layer.
//!
//! An issuer grants a counterparty read on a subset of its claims; the
//! counterparty receives exactly that subset over reconciliation, and the
//! withheld entries never arrive — content or existence. Per
//! `code-practices/access-control-tests.md`, every allowed path sits next
//! to its tightest denial: the outsider, the holder of the replica's
//! ticket without a grant, and (for writes) the read-only grant holder.
//!
//! Establishment (the pairing dialogue) and device-set publication live in
//! pdn-node; here the tickets and records travel by direct handover,
//! exactly the store-level acts the ceremonies perform.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    claim_id_of, AddrInfoOptions, ConnectionMetadataStore, PrivateMetadataStore, ReadGrant,
    ShareMode, SpawnOptions, SyncNode,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, ids};

/// The three claims Bob's data store carries in these scenarios.
const GRANTED: &str = "contact/email";
const WITHHELD_A: &str = "contact/phone";
const WITHHELD_B: &str = "notes/diary";

/// The reconcile cadence these scenarios run at. Scoped readers have no
/// gossip path, so every negative assertion is "the reader retried over
/// several intervals and was refused" — at the production default that is
/// tens of seconds of pure sleep per assertion; injected here it is
/// milliseconds, and the assertions wait out the same number of intervals.
const RECONCILE: Duration = Duration::from_millis(500);

/// Spawn a node with the test's short reconcile cadence.
async fn spawn_node() -> Result<SyncNode> {
    SyncNode::spawn_with_options(SpawnOptions {
        reconcile_interval: RECONCILE,
    })
    .await
}

/// Assemble the serving side: Bob's node hosting his identity — directory
/// with his device registered, data namespace with three entries — plus a
/// connection toward `peer` (both directional stores) registered for
/// caller classification. Returns the connection pair (Bob's `own` toward
/// the peer, and Bob's copy of `peer`'s reverse store) with the read
/// ticket of Bob's own store for the counterparty to import.
struct ServingSide {
    own_toward_peer: ConnectionMetadataStore,
    own_read_ticket: data_layer::DocTicket,
}

async fn serving_side(
    bob: &SyncNode,
    peer: PdnId,
    peer_own: &ConnectionMetadataStore,
) -> Result<ServingSide> {
    // Bob's directory: his identity's device set, Invariant 1 audience.
    let directory = PrivateMetadataStore::create(bob).await?;
    directory.add_device(bob.node_id()).await?;
    bob.host_identity(ids::BOB, &directory)?;

    // The connection pair as establishment leaves it: Bob's own store
    // toward the peer — carrying his published device set (publication is
    // bilateral) — and Bob's imported copy of the peer's reverse store
    // (where the peer publishes its device set).
    let own_toward_peer = ConnectionMetadataStore::create(bob).await?;
    own_toward_peer.publish_device(bob.node_id()).await?;
    let own_read_ticket = own_toward_peer
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let peer_store = ConnectionMetadataStore::import(
        bob,
        peer_own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?,
    )
    .await?;
    bob.host_connection(ids::BOB, peer, &own_toward_peer, &peer_store)?;

    Ok(ServingSide {
        own_toward_peer,
        own_read_ticket,
    })
}

/// Write Bob's three entries into his data namespace.
async fn write_bobs_entries(bob: &SyncNode) -> Result<()> {
    let author = bob.create_author().await?;
    for (path, payload) in [
        (GRANTED, b"bob@example.org".as_slice()),
        (WITHHELD_A, b"+1-555-0100".as_slice()),
        (WITHHELD_B, b"dear diary".as_slice()),
    ] {
        bob.write(ids::BOB, author, &EntryPath::new(path)?, payload)
            .await?;
    }
    Ok(())
}

/// The read-restriction scenario (Invariant 2), allowed and denied sides
/// probed in one place.
///
/// Allowed: Alice, granted read on exactly `contact/email`, receives that
/// entry — and keeps receiving its updates.
///
/// Denied, existence hidden: the withheld entries never reach Alice — not
/// after the grant, and not after a proven second replication wave (the
/// sentinel update) — so her view is indistinguishable from a replica in
/// which they do not exist.
///
/// Denied, ticket without a grant: Carol holds the replica's leaked read
/// ticket but no grant and no connection with Bob; her node obtains
/// nothing — no entry, no listing.
///
/// Denied, read-only cannot write: Alice's ticket carries no namespace
/// secret, so her local write into Bob's namespace fails outright.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn read_restricted_peer_receives_exactly_the_granted_subset() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;
    let carol = spawn_node().await?;

    // Alice's reverse-direction store, carrying her published device set.
    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;

    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    // Bob's data namespace with the three entries.
    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The grant: read on exactly `contact/email`, no write — so the grant
    // ships a read ticket (no namespace secret).
    let email = EntryPath::new(GRANTED)?;
    let grant = ReadGrant {
        issuer: ids::BOB,
        audience: ids::ALICE,
        claims: NonEmpty::new(claim_id_of(&ids::BOB, &email)),
        write: false,
    };
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_scoped_grant(&grant, &data_read_ticket)
        .await?;

    // Alice consumes the grant as the bootstrap cascade would: reads it
    // from her copy of Bob's store, registers her side of the connection
    // for classification (so Bob's devices resolve when they dial her, and
    // her own dials to them are judged), and imports the namespace scoped
    // — outside the replica's gossip swarm, reconciliation is her only
    // data path.
    let alice_peer =
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (received_grant, received_ticket) = eventually_scoped_grant(&alice_peer, ids::BOB).await?;
    assert_eq!(received_grant.claims, grant.claims);
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // Allowed: the granted entry arrives, with its payload.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the granted peer"
    );

    // Denied (read-only cannot write): Alice holds no namespace secret, so
    // a local write into Bob's namespace is refused outright.
    let alice_author = alice.create_author().await?;
    assert!(
        alice
            .write(ids::BOB, alice_author, &email, b"overwrite attempt")
            .await
            .is_err(),
        "a write through a read-only grant must be refused"
    );

    // Denied (ticket without a grant): Carol imports the leaked read
    // ticket. Bob's node cannot resolve her to any granted identity, so
    // her sync requests are refused and nothing ever arrives.
    carol
        .import_namespace_scoped(ids::BOB, data_read_ticket)
        .await?;

    // Sentinel: Bob updates the granted entry. Its arrival at Alice proves
    // a second replication wave ran end-to-end after the negatives were
    // set up — so the absence assertions below are ordered, not racy.
    let author = bob.create_author().await?;
    bob.write(ids::BOB, author, &email, b"bob@new.example.org")
        .await?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@new.example.org"))
        })
        .await?,
        "the sentinel update did not reach the granted peer"
    );

    // Denied (existence hidden): after the proven second wave, Alice's
    // view still lists exactly the granted entry — the withheld entries
    // are absent as records, not merely unreadable.
    let listed: Vec<String> = alice
        .list(ids::BOB, None)
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
                .read(ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a withheld entry leaked to the granted peer: {withheld}"
        );
    }

    // ...and Carol, with the ticket but no grant, has obtained nothing.
    // Her denial is bounded, not incidental: her import fired a sync
    // attempt, every one of her reconcile intervals since re-dials Bob's
    // node (his address rides the leaked ticket), and the reads below
    // nudge once more — waiting out three more of her intervals after the
    // proven second wave means "she tried repeatedly and was refused" is
    // what keeps this green, not a poll that outran her first dial.
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        carol.list(ids::BOB, None).await?.is_empty(),
        "a ticket holder without a grant must obtain nothing"
    );
    assert!(carol.read(ids::BOB, &email).await?.is_none());

    bob.shutdown().await?;
    alice.shutdown().await?;
    carol.shutdown().await?;
    Ok(())
}

/// The write-grant scenario: a grant carrying write ships a write ticket,
/// and the audience's write on the granted claim reaches the issuer —
/// while the read filter still narrows what flows the other way.
///
/// Allowed: Alice, granted read+write on `shared/note`, writes it and Bob
/// converges on her value.
///
/// Denied (the read side is still scoped): Bob's other entries never reach
/// Alice, proven after her own write demonstrably round-tripped.
///
/// KNOWN GAP, pinned: with no ingest hook installed (ADR-0008), write
/// authority is the namespace secret — effectively whole-store — so
/// Alice's write *outside* her granted claim is accepted, and the test
/// asserts exactly that undesired behaviour. A red run here means
/// ungranted writes are rejected; the pinned assertion must then flip into
/// the denial it stands in for.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn write_grant_round_trips_while_reads_stay_scoped() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The shared, writable claim.
    let note = EntryPath::new("shared/note")?;
    let bob_author = bob.create_author().await?;
    bob.write(ids::BOB, bob_author, &note, b"from bob").await?;

    // Grant read+write on exactly `shared/note`; the grant ships a WRITE
    // ticket — the namespace secret carries write authority.
    let grant = ReadGrant {
        issuer: ids::BOB,
        audience: ids::ALICE,
        claims: NonEmpty::new(claim_id_of(&ids::BOB, &note)),
        write: true,
    };
    let data_write_ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_scoped_grant(&grant, &data_write_ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) = eventually_scoped_grant(&alice_peer, ids::BOB).await?;
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // The granted entry arrives at Alice first (so her write below is an
    // update, not a blind create).
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from bob"))
        })
        .await?,
        "the granted entry did not reach the write-granted peer"
    );

    // Allowed: Alice writes the granted claim under her own author, and
    // Bob converges on her value.
    let alice_author = alice.create_author().await?;
    alice
        .write(ids::BOB, alice_author, &note, b"from alice")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from alice"))
        })
        .await?,
        "the write-granted peer's write did not reach the issuer"
    );

    // Denied: the round-trip above proves bidirectional replication ran,
    // yet the ungranted entries still never reached Alice.
    let listed: Vec<String> = alice
        .list(ids::BOB, None)
        .await?
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(
        listed,
        vec!["shared/note".to_owned()],
        "a write grant must not widen the read scope"
    );

    // KNOWN GAP, deliberately asserted (see the test doc): the write side
    // is the namespace secret, so nothing scopes Alice's writes to her
    // granted claim — she writes a path that was never granted (and that
    // the read filter hides from her!), and the issuer accepts it,
    // overwriting his own entry. A red run here means the ADR-0008 gap
    // closed; replace this assertion with the write-denial pair.
    let diary = EntryPath::new(WITHHELD_B)?;
    alice
        .write(ids::BOB, alice_author, &diary, b"ungranted overwrite")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, &diary)
                .await?
                .is_some_and(|p| p == b"ungranted overwrite"))
        })
        .await?,
        "the pinned ADR-0008 gap closed: ungranted writes are now rejected — \
         replace this assertion with the write-denial pair"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// Withdrawal: rights are frozen per session, so a withdrawn grant refuses
/// the *next* session — while data already delivered stays readable
/// (Invariant 2 governs acquisition, not retention).
///
/// Allowed (before): Alice converges on the granted entry.
///
/// Denied (after): once the issuer withdraws the grant, updates stop
/// reaching Alice — probed by writing an update, waiting out several of
/// her reconcile intervals, and asserting her view still carries the
/// pre-withdrawal value; that value itself is still readable.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_grant_refuses_the_next_session_but_keeps_delivered_data() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    let email = EntryPath::new(GRANTED)?;
    let grant = ReadGrant {
        issuer: ids::BOB,
        audience: ids::ALICE,
        claims: NonEmpty::new(claim_id_of(&ids::BOB, &email)),
        write: false,
    };
    let ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_scoped_grant(&grant, &ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) = eventually_scoped_grant(&alice_peer, ids::BOB).await?;
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // Allowed: the granted entry converges before the withdrawal.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org"))
        })
        .await?,
        "the granted entry did not reach the granted peer before withdrawal"
    );

    // The issuer withdraws the grant — one tombstone over the one record,
    // whatever its width; his own book reads it as absent at once, so his
    // next session classification has nothing to admit.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    assert!(serving
        .own_toward_peer
        .read_scoped_grant(ids::BOB)
        .await?
        .is_none());

    // Rights are frozen per session: a session that started just before
    // the withdrawal still carries the granted claim, and if the update
    // landed while such a session was mid-exchange it could ride out
    // legitimately. One interval drains any in-flight pre-withdrawal
    // session (sessions on loopback finish in milliseconds) before the
    // update exists at all, so the assertion below probes only sessions
    // classified after the withdrawal.
    tokio::time::sleep(RECONCILE).await;

    // Denied: an update written after the withdrawal never arrives. Alice's
    // reconcile pass retries every interval; waiting out several of her
    // intervals makes "she tried and was refused" the only way to stay
    // green — an admitted update would flip the assertion red.
    let author = bob.create_author().await?;
    bob.write(ids::BOB, author, &email, b"bob@after-withdrawal")
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        alice
            .read(ids::BOB, &email)
            .await?
            .is_some_and(|p| p == b"bob@example.org"),
        "an update leaked through a withdrawn grant, or delivered data was lost"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// Swarm membership does not bypass the access book. The fork's swarm is
/// content-free: entries never ride the gossip topic, they flow only over
/// the classified reconciliation an announce triggers — so a swarm member
/// is served exactly what the issuer's book grants it *at each session*,
/// never a raw broadcast.
///
/// Dave joins Bob's data-namespace swarm (a device-style import) and stays a
/// member throughout. Positive control: while Bob's book carries a
/// whole-store grant for Dave, Dave converges on Bob's write — proving the
/// mesh is live and the delivery path works. Negative: once Bob withdraws
/// the grant, a write made afterwards never reaches Dave — although Dave is
/// still a swarm member.
#[tokio::test(flavor = "multi_thread")]
async fn swarm_membership_does_not_bypass_the_access_book() -> Result<()> {
    /// How long a would-be broadcast gets before "it never came" counts —
    /// absolute, because gossip latency does not scale with the reconcile
    /// interval.
    const SWARM_WINDOW: Duration = Duration::from_secs(15);

    let bob = spawn_node().await?;
    let dave = spawn_node().await?;

    // Bob's serving side, armed, with a connection toward Dave — so Bob can
    // resolve Dave's node id and carry a grant for him.
    let dave_own = ConnectionMetadataStore::create(&dave).await?;
    dave_own.publish_device(dave.node_id()).await?;
    let serving = serving_side(&bob, ids::DAVE, &dave_own).await?;

    bob.create_namespace(ids::BOB).await?;
    let email = EntryPath::new(GRANTED)?;
    let bob_author = bob.create_author().await?;
    let ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;

    // Dave joins Bob's swarm with a device-style import, and stays a member
    // for the whole test — nothing below removes him from the swarm.
    dave.import_namespace(ids::BOB, ticket).await?;

    // Bob's book carries a whole-store grant for Dave.
    let grant_ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(ids::BOB, &grant_ticket)
        .await?;

    // Positive control: while granted, Dave converges on Bob's write — the
    // mesh is live and content is delivered (through the classified sync,
    // not a raw broadcast, but that distinction is invisible here — only
    // that it arrives). Bob re-writes each poll so a first announce lands.
    assert!(
        eventually(|| async {
            bob.write(ids::BOB, bob_author, &email, b"bob@example.org")
                .await?;
            Ok(dave.read(ids::BOB, &email).await?.is_some())
        })
        .await?,
        "the granted swarm member did not converge — mesh/positive control failed"
    );

    // Bob withdraws the grant; his own book reads it as absent at once.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    // Drain any pre-withdrawal session (rights are frozen per session)
    // before the probe write exists, so the negative probes only sessions
    // classified after the withdrawal.
    tokio::time::sleep(RECONCILE).await;

    // Negative: a write made after the withdrawal never reaches Dave —
    // although he is still a swarm member.
    let after = EntryPath::new(WITHHELD_A)?;
    bob.write(ids::BOB, bob_author, &after, b"post-withdrawal")
        .await?;
    tokio::time::sleep(SWARM_WINDOW).await;
    assert!(
        dave.read(ids::BOB, &after).await?.is_none(),
        "a swarm member received a write after its grant was withdrawn — the swarm carried content"
    );
    // What was delivered while granted is retained (acquisition, not
    // retention), so the negative above is not a wiped replica.
    assert!(dave.read(ids::BOB, &email).await?.is_some());

    bob.shutdown().await?;
    dave.shutdown().await?;
    Ok(())
}

/// Poll the peer store until the scoped grant for `issuer` is readable
/// (record and payloads arrived), then return it.
async fn eventually_scoped_grant(
    store: &ConnectionMetadataStore,
    issuer: PdnId,
) -> Result<(ReadGrant, data_layer::DocTicket)> {
    let mut found = None;
    let ok = eventually(|| async { Ok(store.read_scoped_grant(issuer).await?.is_some()) }).await?;
    if ok {
        found = store.read_scoped_grant(issuer).await?;
    }
    found.ok_or_else(|| anyhow::anyhow!("scoped grant for {issuer} did not arrive"))
}
