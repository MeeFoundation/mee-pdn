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
    claim_id_of, AddrInfoOptions, ConnectionMetadataStore, GrantRead, GrantedClaim,
    PrivateMetadataStore, ReadGrant, ShareMode, SpawnOptions, SyncNode,
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

/// Assemble the serving side: Bob's node hosting his identity — directory
/// with his device registered, data namespace with three entries — plus a
/// connection toward `peer` (both directional stores) registered for
/// caller classification. Returns the connection pair (Bob's `own` toward
/// the peer, and Bob's copy of `peer`'s reverse store) with the read
/// ticket of Bob's own store for the counterparty to import.
struct ServingSide {
    own_toward_peer: ConnectionMetadataStore,
    own_read_ticket: data_layer::DocTicket,
    /// Bob's directory — the device set the access book probes, handed back
    /// for the scenarios that vary who is in it.
    directory: PrivateMetadataStore,
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
        directory,
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
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let data_read_ticket = bob
        .share_ticket(
            ids::BOB,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &data_read_ticket)
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
    let (received_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
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

/// A pending device registration confers nothing; the confirmation is the
/// whole difference. Linking registers its newcomer before it can know the
/// reply arrived, so what it writes is pending — and the access book probes
/// the confirmed set alone. Alice, a scoped grantee of Bob who is also
/// pending in his directory, receives exactly her granted claim: the
/// registration adds not one entry to what her grant already allows. The
/// same node, once Bob's directory carries its confirmation, reads the
/// replica whole — so the denial above is the record's doing, not a path
/// that was never live.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_device_registration_confers_nothing() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The registration a linking dialogue leaves on the inviter before —
    // and, when the reply is lost, instead of — the newcomer's own
    // confirmation.
    serving
        .directory
        .add_pending_device(alice.node_id())
        .await?;

    // Alice's grant on one claim, consumed the way the bootstrap does.
    let email = EntryPath::new(GRANTED)?;
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let data_read_ticket = bob
        .share_ticket(
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
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // Allowed, by the grant alone: the granted entry arrives, which also
    // proves the reconciliation path is live for the denial below.
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

    // Denied: pending is not membership. Waited out over three more of
    // Alice's reconcile intervals after the granted entry proved the path
    // live, so this is "she reconciled repeatedly and was served nothing
    // more", not a poll that outran her first session.
    tokio::time::sleep(RECONCILE * 3).await;
    for withheld in [WITHHELD_A, WITHHELD_B] {
        assert!(
            alice
                .read(ids::BOB, &EntryPath::new(withheld)?)
                .await?
                .is_none(),
            "a pending registration served a withheld entry: {withheld}"
        );
    }

    // The confirmation, and nothing else about Alice, changes: she is a
    // device of Bob's identity now, and the whole replica is hers.
    serving.directory.confirm_device(alice.node_id()).await?;
    for withheld in [WITHHELD_A, WITHHELD_B] {
        let path = EntryPath::new(withheld)?;
        assert!(
            eventually(|| async { Ok(alice.read(ids::BOB, &path).await?.is_some()) }).await?,
            "a confirmed device must be served the replica whole: {withheld}"
        );
    }

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// The grant's capability names its audience, and the serving side honors
/// that name — not the mere position of the record. A grant sitting in
/// Bob's store toward Alice but whose capability names Carol as its
/// audience serves Alice nothing: the record's place says who wrote it, the
/// capability says whom it was written for, and only the second authorizes.
///
/// This is the one guard between "a device the connection is toward" and "a
/// device the grant is addressed to". Without the `cap.audience` check the
/// classifier would extend Alice the claim set on position alone, so a
/// record misaddressed — by a bug, or by a replica shared into two
/// connections — would serve the wrong identity's devices.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_addressed_to_another_identity_serves_nobody() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // Published into Bob's store toward Alice — but the capability names
    // Carol, not Alice, as its audience.
    let email = EntryPath::new(GRANTED)?;
    let misaddressed = granted(ids::BOB, ids::CAROL, &email, false);
    let data_read_ticket = bob
        .share_ticket(
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
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    // The grant record converges to Alice — proof the two nodes replicate,
    // so the data denial below is "refused", not "not yet connected". She
    // reads no grant out of it all the same: the record sits in the store
    // Bob writes toward her, but only the capability says whom it was
    // written for, and it names Carol.
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
        .import_namespace_scoped(ids::BOB, data_read_ticket)
        .await?;

    // Bob updates the claim; Alice re-dials Bob every reconcile interval,
    // and after three past a fresh write "she asked repeatedly and was
    // refused" is what holds this green.
    let author = bob.create_author().await?;
    bob.write(ids::BOB, author, &email, b"bob@new.example.org")
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        alice.list(ids::BOB, None).await?.is_empty(),
        "a grant naming another identity's audience must serve nothing"
    );
    assert!(alice.read(ids::BOB, &email).await?.is_none());

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// The write-grant scenario: a grant carrying write ships a write ticket,
/// the audience's write on the granted claim reaches the issuer — and the
/// ingest gate (ADR-0008) bounds the secret to the granted claim — while
/// the read filter still narrows what flows the other way.
///
/// Allowed: Alice, granted read+write on `shared/note`, writes it and Bob
/// converges on her value.
///
/// Denied (the read side is still scoped): Bob's other entries never reach
/// Alice, proven after her own write demonstrably round-tripped.
///
/// Denied (write outside the write set): the ticket's secret lets Alice
/// produce an entry at a claim that was never granted, and the issuer's
/// gate refuses it — Bob's own entry survives, proven after a second
/// granted-claim round-trip ordered the denial.
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
    // ticket — the namespace secret is the transport of write authority,
    // and the ingest gate is what scopes it.
    let grant = granted(ids::BOB, ids::ALICE, &note, true);
    let data_write_ticket = bob
        .share_ticket(
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
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
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

    // Denied (write outside the write set): the secret signs an entry at a
    // path that was never granted (and that the read filter hides from
    // her). The gate refuses it at every device of the issuer, so Bob's
    // own entry survives; her replica keeps her value — a provisional
    // write, whose bounded fate is the retraction discipline's.
    let diary = EntryPath::new(WITHHELD_B)?;
    alice
        .write(ids::BOB, alice_author, &diary, b"ungranted overwrite")
        .await?;
    // Sentinel: a second granted-claim round-trip proves the sessions that
    // carried — and refused — the ungranted write have run.
    alice
        .write(ids::BOB, alice_author, &note, b"from alice again")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from alice again"))
        })
        .await?,
        "the sentinel granted-claim write did not round-trip"
    );
    assert!(
        bob.read(ids::BOB, &diary)
            .await?
            .is_some_and(|p| p == b"dear diary"),
        "an ungranted write leaked through the ingest gate"
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
    let grant = granted(ids::BOB, ids::ALICE, &email, false);
    let ticket = bob
        .share_ticket(
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
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
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
        .read_grant(ids::BOB, ids::ALICE)
        .await?
        .granted()
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
/// grant for Dave, Dave converges on Bob's write — proving the
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

    // Bob's book carries a grant for Dave on the granted claim.
    let grant_ticket = bob
        .share_ticket(
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

/// One grant, mixed rights: a read-only claim beside a read-write claim
/// over one connection. Allowed: the write-granted claim round-trips under
/// the audience's author. Denied: the same holder's write at the read-only
/// claim — produced with the very secret the write ticket carries — never
/// reaches the issuer, and the issuer's value survives, ordered by a
/// sentinel wave on the writable claim.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, allowed and denied sides in one place
async fn a_mixed_grant_admits_exactly_its_write_claims() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    write_bobs_entries(&bob).await?;

    // The mixed grant: `contact/email` read-only, `contact/phone`
    // read-write — one record, a write ticket (write present).
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
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    serving
        .own_toward_peer
        .publish_grant(&grant, &write_ticket)
        .await?;

    let alice_peer =
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // Both granted claims arrive.
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &email)
                .await?
                .is_some_and(|p| p == b"bob@example.org")
                && alice
                    .read(ids::BOB, &phone)
                    .await?
                    .is_some_and(|p| p == b"+1-555-0100"))
        })
        .await?,
        "the granted claims did not reach the mixed-grant audience"
    );

    // Allowed: the write-granted claim round-trips.
    let alice_author = alice.create_author().await?;
    alice
        .write(ids::BOB, alice_author, &phone, b"+7-999-0001")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, &phone)
                .await?
                .is_some_and(|p| p == b"+7-999-0001"))
        })
        .await?,
        "the write-granted claim did not round-trip"
    );

    // Denied: the read-only claim, forced with the ticket's secret. The
    // sentinel wave on the writable claim proves the sessions that carried
    // — and refused — the forged entry have run.
    alice
        .write(ids::BOB, alice_author, &email, b"forged@alice")
        .await?;
    alice
        .write(ids::BOB, alice_author, &phone, b"+7-999-0002")
        .await?;
    assert!(
        eventually(|| async {
            Ok(bob
                .read(ids::BOB, &phone)
                .await?
                .is_some_and(|p| p == b"+7-999-0002"))
        })
        .await?,
        "the sentinel write on the writable claim did not round-trip"
    );
    assert!(
        bob.read(ids::BOB, &email)
            .await?
            .is_some_and(|p| p == b"bob@example.org"),
        "a write at a read-only claim leaked through the ingest gate"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
    Ok(())
}

/// Withdrawal closes the write side from the next session: the same claim
/// accepts the audience's write before the withdrawal and refuses one made
/// after — while the value delivered before stays (acquisition, not
/// retention, both ways).
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_write_grant_refuses_the_next_sessions_writes() -> Result<()> {
    let bob = spawn_node().await?;
    let alice = spawn_node().await?;

    let alice_own = ConnectionMetadataStore::create(&alice).await?;
    alice_own.publish_device(alice.node_id()).await?;
    let serving = serving_side(&bob, ids::ALICE, &alice_own).await?;

    bob.create_namespace(ids::BOB).await?;
    let note = EntryPath::new("shared/note")?;
    let bob_author = bob.create_author().await?;
    bob.write(ids::BOB, bob_author, &note, b"from bob").await?;

    let grant = granted(ids::BOB, ids::ALICE, &note, true);
    let write_ticket = bob
        .share_ticket(
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
        ConnectionMetadataStore::import(&alice, serving.own_read_ticket.clone()).await?;
    alice.host_connection(ids::ALICE, ids::BOB, &alice_own, &alice_peer)?;
    let (_grant, received_ticket) =
        eventually_scoped_grant(&alice_peer, ids::BOB, ids::ALICE).await?;
    alice
        .import_namespace_scoped(ids::BOB, received_ticket)
        .await?;

    // Accepted while granted.
    let alice_author = alice.create_author().await?;
    assert!(
        eventually(|| async {
            Ok(alice
                .read(ids::BOB, &note)
                .await?
                .is_some_and(|p| p == b"from bob"))
        })
        .await?,
        "the granted claim did not arrive before the withdrawal"
    );
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
        "the pre-withdrawal write was not accepted"
    );

    // Withdrawn: rights are frozen per session, so one interval drains any
    // in-flight pre-withdrawal session before the probe write exists.
    serving.own_toward_peer.withdraw_grant(ids::BOB).await?;
    tokio::time::sleep(RECONCILE).await;

    // A write made after the withdrawal never reaches the issuer — her
    // node retries every interval and is refused each time — while the
    // value accepted while granted stays.
    alice
        .write(ids::BOB, alice_author, &note, b"post-withdrawal")
        .await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        bob.read(ids::BOB, &note)
            .await?
            .is_some_and(|p| p == b"from alice"),
        "a write leaked through a withdrawn grant, or the accepted value was lost"
    );

    bob.shutdown().await?;
    alice.shutdown().await?;
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
