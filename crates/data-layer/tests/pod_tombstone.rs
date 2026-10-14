//! A departed member's devices and the pod's membership store they keep as
//! its tombstone: served by member devices over the departure's past alone,
//! both ways, refused the record store, served whole by a sibling, and
//! reconciled with member devices until one session with them goes
//! through, then with the identity's own devices alone. The entries a
//! pod's creation, its joins and its departures write arrive by the
//! store-level writes the pods service performs, and the tickets by hand.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use data_layer::{
    identity_of, AddrInfoOptions, AuthorId, Contact, EventKind, MemberDevice, MemberState,
    MembershipKey, NamespaceId, PodStore, PrivateMetadataStore, Seq, SessionTrace, ShareMode,
    SpawnOptions, SyncNode, Traced,
};
use pdn_types::PodId;
use test_utils::{
    join_identity,
    pod::{
        create, device_of, held_records, host, invite, lists, lists_device, place_claim, reads,
        state_on, tickets, write, Person,
    },
    wait_devices, TIMEOUT,
};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);
/// A pod pass short enough that a scenario waits a few runs at most.
const POD_RUN: Duration = Duration::from_millis(300);

const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};
const OUT: MemberState = MemberState {
    member: false,
    owner: false,
};

async fn node(pod_reconcile_interval: Duration) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval,
        ..SpawnOptions::memory()
    })
    .await
}

async fn node_on(dir: &std::path::Path) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: QUIET,
        ..SpawnOptions::on_directory(dir)
    })
    .await
}

/// A device no node runs: a statement may list it, and nothing answers a
/// dial to it.
fn nowhere(seed: u8) -> MemberDevice {
    let key = iroh::SecretKey::from_bytes(&[seed; 32]).public();
    MemberDevice {
        node: pdn_types::NodeId::from_bytes(*key.as_bytes()),
        author: AuthorId::from([seed; 32]),
    }
}

async fn depart(
    node: &SyncNode,
    member: &Person,
    pod: PodId,
    kind: EventKind,
    by: &Person,
    seq: u64,
) -> Result<()> {
    let key = MembershipKey::Event {
        subject: member.id,
        seq: Seq::new(seq),
        kind,
        actor: by.id,
        actor_seq: Seq::FIRST,
    };
    write(node, by, pod, key, vec![0]).await
}

async fn knows(node: &SyncNode, holder: &Person, pod: PodId, member: &Person) -> Result<bool> {
    Ok(node
        .pod_membership_view(holder.id, pod)
        .await?
        .member(&member.id)
        .is_some())
}

/// A dial of one of `pod`'s stores from `holder`'s replica on `from` to
/// `callee`'s on `to`, as a drawn contact is dialed.
async fn dial(
    from: &SyncNode,
    holder: &Person,
    pod: PodId,
    store: PodStore,
    to: &SyncNode,
    callee: &Person,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee.id));
    from.sync_pod_with_for_test(holder.id, pod, store, contact)
        .await
}

/// Whether `pod`'s stores come to have nothing in flight on every one of
/// `nodes` — no exchange running, held or due to redial — in two reads in
/// a row: a dial one node still makes lands on another between two reads.
/// Out of a swarm, a store is still dialed by every node it had a session
/// with until then.
async fn settle(nodes: &[&SyncNode], pod: PodId) -> Result<bool> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut quiet_reads = 0_u8;
    while std::time::Instant::now() < deadline {
        let mut in_flight = 0_usize;
        for node in nodes {
            in_flight = in_flight.saturating_add(node.pod_syncs_in_flight_for_test(pod).await?);
        }
        quiet_reads = if in_flight == 0 {
            quiet_reads.saturating_add(1)
        } else {
            0
        };
        if quiet_reads == 2 {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// Every trace on `traces` whose session has `carol` on `c1` as either
/// party, oldest first, as `+ms book role store remote@peer: what`.
fn sessions_with(
    traces: &mut [tokio::sync::mpsc::UnboundedReceiver<SessionTrace>],
    since: Instant,
    carol: &Person,
    c1: pdn_types::NodeId,
    names: &HashMap<[u8; 32], &str>,
) -> Vec<String> {
    let name = |bytes: &[u8; 32]| {
        names.get(bytes).map_or_else(
            || {
                bytes.iter().take(5).fold(String::new(), |mut short, byte| {
                    let _ = write!(short, "{byte:02x}");
                    short
                })
            },
            |name| (*name).to_owned(),
        )
    };
    let mut held = Vec::new();
    for receiver in traces.iter_mut() {
        while let Ok(trace) = receiver.try_recv() {
            held.push(trace);
        }
    }
    held.sort_by_key(|trace| trace.at);
    held.iter()
        .filter(|trace| trace.identity == carol.id || trace.peer == c1)
        .map(|trace| {
            let what = match &trace.traced {
                Traced::Admitted { key, author } => format!(
                    "admitted {} by {}",
                    String::from_utf8_lossy(key),
                    author.fmt_short()
                ),
                decision => format!("{decision:?}"),
            };
            format!(
                "+{}ms {} {:?} {} {}@{}: {what}",
                trace.at.saturating_duration_since(since).as_millis(),
                name(trace.identity.as_bytes()),
                trace.role,
                name(trace.namespace.as_bytes()),
                name(trace.remote.as_bytes()),
                name(trace.peer.as_bytes()),
            )
        })
        .collect()
}

/// Alice's pod with Bob and Carol, Carol's device out of both swarms, so its
/// own view still lists her a member when a session arrives, as a device
/// offline throughout does, and every node settled: a dial of Carol's
/// device's crossing a held session would supersede it by the tie-break.
struct StaleMember {
    alice_phone: SyncNode,
    bob_phone: SyncNode,
    carol_phone: SyncNode,
    alice: Person,
    carol: Person,
    pod: PodId,
    records: NamespaceId,
}

async fn stale_member() -> Result<StaleMember> {
    let (alice_phone, bob_phone, carol_phone) =
        (node(QUIET).await?, node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    assert!(lists(&carol_phone, carol.id, pod, bob.id, PLAIN).await?);
    let (membership, records) = carol_phone.pod_stores(carol.id, pod)?;
    for namespace in [membership, records] {
        carol_phone
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
    }
    assert!(settle(&[&alice_phone, &bob_phone, &carol_phone], pod).await?);
    Ok(StaleMember {
        alice_phone,
        bob_phone,
        carol_phone,
        alice,
        carol,
        pod,
        records,
    })
}

/// A member device's dial whose access was decided while a member was still
/// one, and whose session reaches that member's device only after its
/// removal and a record placed after it, takes the record to it nowhere.
///
/// The dial is held right after its access decision, as long as a connect
/// to a device that is down would hold a decision taken before it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dial_decided_before_a_removal_takes_nothing_placed_after_it() -> Result<()> {
    let StaleMember {
        alice_phone,
        bob_phone,
        carol_phone,
        alice,
        carol,
        pod,
        records,
    } = stale_member().await?;
    let mut accepted = carol_phone
        .watch_pod_sessions(carol.id, pod, PodStore::Records)
        .await?;
    let held_dial =
        alice_phone.pause_next_dial_for_test(alice.id, records, carol_phone.node_id())?;
    dial(
        &alice_phone,
        &alice,
        pod,
        PodStore::Records,
        &carol_phone,
        &carol,
    )
    .await?;
    held_dial.reached.notified().await;
    depart(&alice_phone, &carol, pod, EventKind::Removed, &alice, 2).await?;
    place_claim(&alice_phone, &alice, pod, 1).await?;
    held_dial.release.notify_one();
    accepted
        .next_with(alice_phone.node_id(), false, TIMEOUT)
        .await?
        .context("the held dial's session never reached the device")?;

    let held = held_records(&carol_phone, carol.id, pod).await?;
    assert!(
        held.is_empty(),
        "the removed member's device holds records: {held:#?}"
    );

    for node in [alice_phone, bob_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A session a removed member's device dials, accepted on a decision taken
/// before the removal, takes nothing placed after the removal to it.
///
/// The accept is held right after its access decision, as long as the
/// engine's queues on a loaded node would hold it before its snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn an_accept_decided_before_a_removal_takes_nothing_placed_after_it() -> Result<()> {
    let StaleMember {
        alice_phone,
        bob_phone,
        carol_phone,
        alice,
        carol,
        pod,
        records,
    } = stale_member().await?;
    let mut dialed = carol_phone
        .watch_pod_sessions(carol.id, pod, PodStore::Records)
        .await?;
    let held_accept =
        alice_phone.pause_next_accept_for_test(alice.id, records, carol_phone.node_id())?;
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Records,
        &alice_phone,
        &alice,
    )
    .await?;
    held_accept.reached.notified().await;
    depart(&alice_phone, &carol, pod, EventKind::Removed, &alice, 2).await?;
    place_claim(&alice_phone, &alice, pod, 1).await?;
    held_accept.release.notify_one();
    dialed
        .next_with(alice_phone.node_id(), true, TIMEOUT)
        .await?
        .context("the held session never ended on the device")?;

    let held = held_records(&carol_phone, carol.id, pod).await?;
    assert!(
        held.is_empty(),
        "the removed member's device holds records: {held:#?}"
    );

    for node in [alice_phone, bob_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A device offline while its member is removed learns of the removal at its
/// first session with a member device, taking the removal and what it rests
/// on, though an entry counting for nothing sits at its member's next
/// sequence. Denied: a membership event outside the removal's past and a
/// record placed after it reach the device from no member device, the
/// record store refused to it.
#[allow(clippy::too_many_lines)] // one scenario: the removal while offline, the return and each denial
#[tokio::test(flavor = "multi_thread")]
async fn a_device_offline_during_its_members_removal_learns_of_the_removal_and_nothing_after(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (alice_phone, bob_phone) = (node(QUIET).await?, node(QUIET).await?);
    let carol_phone = node_on(dir.path()).await?;
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    assert!(lists(&carol_phone, carol.id, pod, bob.id, PLAIN).await?);
    assert!(lists(&bob_phone, bob.id, pod, carol.id, PLAIN).await?);
    carol_phone.shutdown().await?;
    drop(carol_phone);

    depart(&alice_phone, &carol, pod, EventKind::Removed, &alice, 2).await?;
    // A second removal, as a device of an owner that has not seen the first
    // writes it.
    depart(&alice_phone, &carol, pod, EventKind::Removed, &alice, 3).await?;
    let dave = Person::generate();
    invite(&bob_phone, &bob, pod, &dave, vec![nowhere(0xd0)]).await?;
    let claim = place_claim(&alice_phone, &alice, pod, 1).await?;
    assert!(lists(&bob_phone, bob.id, pod, carol.id, OUT).await?);
    assert!(lists(&alice_phone, alice.id, pod, dave.id, PLAIN).await?);
    assert!(reads(&bob_phone, bob.id, pod, claim).await?);

    let restarted = Instant::now();
    let mut traces = vec![
        alice_phone.take_session_traces().context("taken once")?,
        bob_phone.take_session_traces().context("taken once")?,
    ];
    let carol_phone = node_on(dir.path()).await?;
    traces.push(carol_phone.take_session_traces().context("taken once")?);
    let names: HashMap<[u8; 32], &str> = HashMap::from([
        (*alice_phone.node_id().as_bytes(), "a1"),
        (*bob_phone.node_id().as_bytes(), "b1"),
        (*carol_phone.node_id().as_bytes(), "c1"),
        (*alice.id.as_bytes(), "alice"),
        (*bob.id.as_bytes(), "bob"),
        (*carol.id.as_bytes(), "carol"),
        (*tickets.membership.capability.id().as_bytes(), "membership"),
        (*tickets.records.capability.id().as_bytes(), "records"),
    ]);
    carol_phone.provision_identity(carol.id).await?;
    carol_phone.import_pod(carol.id, pod, tickets).await?;
    assert!(
        lists(&carol_phone, carol.id, pod, carol.id, OUT).await?,
        "the device did not learn of its member's removal"
    );
    let mut records = carol_phone
        .watch_pod_sessions(carol.id, pod, PodStore::Records)
        .await?;
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = records
        .next_with(bob_phone.node_id(), true, TIMEOUT)
        .await?;
    // Denied: the record store, and everything outside the removal's past.
    assert!(
        refused.is_some_and(|session| session.exchanged.is_err()),
        "a member device served the removed member's record store"
    );
    assert!(!knows(&carol_phone, &carol, pod, &dave).await?);
    let held = held_records(&carol_phone, carol.id, pod).await?;
    assert!(
        held.is_empty(),
        "the removed member's device holds records: {held:#?}\nsessions with c1 since its restart: {:#?}",
        sessions_with(&mut traces, restarted, &carol, carol_phone.node_id(), &names)
    );

    for node in [alice_phone, bob_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};

/// The state the membership view of `holder`'s replica on `node`, a
/// tombstone's included, shows `member` in.
async fn standing(node: &SyncNode, holder: &Person, pod: PodId, member: &Person) -> MemberState {
    match node.pod_holding(holder.id, pod).await {
        Ok(Some(holding)) => holding
            .membership
            .member(&member.id)
            .map(|member| member.state)
            .unwrap_or_default(),
        Ok(None) | Err(_) => MemberState::default(),
    }
}

/// A tombstone whose departure a later event reverses takes that event
/// from one dial, and what the event rests on in the sessions it dials
/// again at once, until its membership view shows its member back. Paired
/// denial: a tombstone whose departure stands takes no event of a
/// newcomer's chain from a member device that serves it whole.
///
/// No pass runs and both tombstones are out of their swarm, so the sessions
/// after each dial are the tombstone's own. Carol's promotion is written
/// once Dave's replica is a tombstone, so the removal of Bob that reverses
/// Dave's removal rests on a point of Carol's chain the tombstone does not
/// hold. Carol's device serves Frank's tombstone whole, as a modified device
/// that judges no caller does, so the tombstone's own admission is the one
/// that holds. Sentinel: Alice's second statement, which Frank's tombstone
/// takes from the session that offers the newcomer's join beside it.
#[allow(clippy::too_many_lines)] // one scenario: the removals, the reversal and the two dials
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_takes_what_reverses_its_departure_layer_after_layer() -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET).await?, node(QUIET).await?);
    let (carol_phone, dave_phone) = (node(QUIET).await?, node(QUIET).await?);
    let frank_phone = node(QUIET).await?;
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (frank, _) = host(&frank_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    let members = [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&dave_phone, &dave),
        (&frank_phone, &frank),
    ];
    for (phone, member) in members {
        invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in members {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    for (phone, holder) in members {
        for (_, other) in members {
            assert!(lists(phone, holder.id, pod, other.id, PLAIN).await?);
        }
    }

    depart(&alice_phone, &frank, pod, EventKind::Removed, &alice, 2).await?;
    assert!(lists(&frank_phone, frank.id, pod, frank.id, OUT).await?);
    frank_phone.forget_pod(frank.id, pod).await?;
    let promotion = |subject: &Person, seq| MembershipKey::Event {
        subject: subject.id,
        seq: Seq::new(seq),
        kind: EventKind::Promoted,
        actor: alice.id,
        actor_seq: Seq::FIRST,
    };
    write(&alice_phone, &alice, pod, promotion(&bob, 2), vec![0]).await?;
    assert!(lists(&bob_phone, bob.id, pod, bob.id, OWNER).await?);
    let removal = MembershipKey::Event {
        subject: dave.id,
        seq: Seq::new(2),
        kind: EventKind::Removed,
        actor: bob.id,
        actor_seq: Seq::new(2),
    };
    write(&bob_phone, &bob, pod, removal, vec![0]).await?;
    assert!(lists(&dave_phone, dave.id, pod, dave.id, OUT).await?);
    dave_phone.forget_pod(dave.id, pod).await?;

    write(&alice_phone, &alice, pod, promotion(&carol, 2), vec![0]).await?;
    assert!(lists(&carol_phone, carol.id, pod, carol.id, OWNER).await?);
    let erin = Person::generate();
    invite(&alice_phone, &alice, pod, &erin, vec![nowhere(0xe0)]).await?;
    let second = nowhere(0xa2);
    let alices = vec![device_of(&alice_phone, &alice)?, second];
    let statement = alice.keys.device_statement(&pod, 2, alices).encode();
    let alices_second = MembershipKey::Devices {
        member: alice.id,
        version: 2,
    };
    write(&alice_phone, &alice, pod, alices_second, statement).await?;
    let reversal = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(2),
        kind: EventKind::Removed,
        actor: carol.id,
        actor_seq: Seq::new(2),
    };
    write(&carol_phone, &carol, pod, reversal, vec![0]).await?;
    assert!(lists(&carol_phone, carol.id, pod, dave.id, PLAIN).await?);
    assert!(lists(&carol_phone, carol.id, pod, erin.id, PLAIN).await?);
    assert!(lists_device(&carol_phone, carol.id, pod, alice.id, second).await?);
    assert_eq!(standing(&dave_phone, &dave, pod, &dave).await, OUT);

    dial(
        &dave_phone,
        &dave,
        pod,
        PodStore::Membership,
        &carol_phone,
        &carol,
    )
    .await?;
    let back = test_utils::eventually(|| async {
        Ok(standing(&dave_phone, &dave, pod, &dave).await == PLAIN)
    })
    .await?;
    assert!(
        back,
        "the tombstone did not take what the reversal rests on"
    );
    assert_eq!(standing(&dave_phone, &dave, pod, &bob).await, OUT);

    // Denied: the newcomer's join, to the tombstone whose departure stands.
    carol_phone.serve_pod_sessions_whole_for_test(carol.id, true)?;
    dial(
        &frank_phone,
        &frank,
        pod,
        PodStore::Membership,
        &carol_phone,
        &carol,
    )
    .await?;
    let sentinel = test_utils::eventually(|| async {
        Ok(frank_phone
            .pod_holding(frank.id, pod)
            .await?
            .and_then(|holding| {
                holding
                    .membership
                    .member(&alice.id)
                    .map(|member| member.devices.contains(&second))
            })
            .unwrap_or(false))
    })
    .await?;
    assert!(
        sentinel,
        "the standing tombstone took nothing from the dial"
    );
    assert_eq!(frank_phone.pod_chain_run(frank.id, pod, erin.id).await?, 0);
    assert_eq!(standing(&frank_phone, &frank, pod, &frank).await, OUT);

    for node in [alice_phone, bob_phone, carol_phone, dave_phone, frank_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A leave written with no member device reachable reaches the members at
/// the leaving device's first session with one of them, and its sibling
/// takes it from it. Denied: the leaving device takes nothing outside its
/// leave's past from a member device that does not know of the leave yet,
/// and is refused the record store.
#[allow(clippy::too_many_lines)] // one scenario: the leave, its arrival, the sibling and each denial
#[tokio::test(flavor = "multi_thread")]
async fn a_leave_written_offline_reaches_the_members_and_takes_nothing_after_it() -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET).await?, node(QUIET).await?);
    let (carol_phone, carol_laptop) = (node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = linked(&carol_phone, &carol_laptop).await?;
    let pod = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        pod,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let carols = vec![
        device_of(&carol_phone, &carol)?,
        device_of(&carol_laptop, &carol)?,
    ];
    invite(&alice_phone, &alice, pod, &carol, carols).await?;
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (device, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&carol_laptop, &carol),
    ] {
        device.import_pod(holder.id, pod, tickets.clone()).await?;
        assert!(lists(device, holder.id, pod, bob.id, PLAIN).await?);
    }
    // Out of both swarms and settled, so the leave goes out in the sessions
    // dialed below.
    for namespace in [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ] {
        carol_phone
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
        carol_laptop
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
    }
    let phones = [&alice_phone, &bob_phone, &carol_phone, &carol_laptop];
    assert!(settle(&phones, pod).await?);

    depart(&carol_phone, &carol, pod, EventKind::Left, &carol, 2).await?;
    let dave = Person::generate();
    invite(&bob_phone, &bob, pod, &dave, vec![nowhere(0xd0)]).await?;
    assert!(lists(&bob_phone, bob.id, pod, dave.id, PLAIN).await?);
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Membership,
        &bob_phone,
        &bob,
    )
    .await?;
    assert!(
        lists(&bob_phone, bob.id, pod, carol.id, OUT).await?,
        "the leave did not reach a member's device"
    );
    dial(
        &carol_laptop,
        &carol,
        pod,
        PodStore::Membership,
        &carol_phone,
        &carol,
    )
    .await?;
    assert!(lists(&carol_laptop, carol.id, pod, carol.id, OUT).await?);

    // Denied: the record store; and Dave's joined event, which Bob's phone
    // served whole, not knowing of the leave at that session's setup.
    let mut records = carol_phone
        .watch_pod_sessions(carol.id, pod, PodStore::Records)
        .await?;
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = records
        .next_with(bob_phone.node_id(), true, TIMEOUT)
        .await?;
    assert!(refused.is_some_and(|session| session.exchanged.is_err()));
    assert!(!knows(&carol_phone, &carol, pod, &dave).await?);

    for node in [alice_phone, bob_phone, carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A tombstone that has reached a member's device is reconciled with the
/// identity's own devices alone, and no member's device dials it after:
/// neither side's pass reaches the other, though each had sessions with it.
///
/// What is waited for is a window of three runs of both passes in which no
/// session with the departed device starts: a dial either side asked for
/// before its contacts left the other out is redialed for a few seconds
/// after, while a run that drew the other would start one in every window.
#[allow(clippy::too_many_lines)] // one scenario: the leave, the contacts before and after, and the runs past it
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_is_reconciled_with_its_siblings_alone_once_it_reached_a_member() -> Result<()>
{
    let (alice_phone, bob_phone) = (node(POD_RUN).await?, node(QUIET).await?);
    let (carol_phone, carol_laptop) = (node(POD_RUN).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = linked(&carol_phone, &carol_laptop).await?;
    let pod = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        pod,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let carols = vec![
        device_of(&carol_phone, &carol)?,
        device_of(&carol_laptop, &carol)?,
    ];
    invite(&alice_phone, &alice, pod, &carol, carols).await?;
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (device, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&carol_laptop, &carol),
    ] {
        device.import_pod(holder.id, pod, tickets.clone()).await?;
        assert!(lists(device, holder.id, pod, bob.id, PLAIN).await?);
    }
    let laptop = data_layer::EndpointId::from_bytes(carol_laptop.node_id().as_bytes())?;
    let sibling_only = HashSet::from([(laptop, identity_of(carol.id))]);
    let mut draws = carol_phone
        .take_pod_pass_draws()
        .expect("the draw channel is taken once");
    let mut alice_draws = alice_phone
        .take_pod_pass_draws()
        .expect("the draw channel is taken once");
    let contacts_of = |draw: &data_layer::PodPassDraw| -> HashSet<_> {
        draw.contacts
            .iter()
            .map(|contact| (contact.addr.id, contact.identity))
            .collect()
    };
    // Members' devices among the contacts while a member.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            if draw.store == PodStore::Membership && contacts_of(&draw).len() > 1 {
                break;
            }
        }
    })
    .await?;

    depart(&carol_phone, &carol, pod, EventKind::Left, &carol, 2).await?;
    carol_phone.forget_pod(carol.id, pod).await?;
    assert!(lists(&bob_phone, bob.id, pod, carol.id, OUT).await?);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            if draw.store == PodStore::Membership && contacts_of(&draw) == sibling_only {
                break;
            }
        }
    })
    .await?;

    let carol_on_phone = (
        data_layer::EndpointId::from_bytes(carol_phone.node_id().as_bytes())?,
        identity_of(carol.id),
    );
    assert!(lists(&alice_phone, alice.id, pod, carol.id, OUT).await?);
    assert!(draws_contact(&mut alice_draws, carol_on_phone, false).await?);
    let mut inviter_sessions = alice_phone
        .watch_pod_sessions(alice.id, pod, PodStore::Membership)
        .await?;
    let quiet = tokio::time::timeout(TIMEOUT, async {
        loop {
            let since = std::time::SystemTime::now();
            for runs in [&mut draws, &mut alice_draws] {
                while runs.try_recv().is_ok() {}
                if !membership_runs(runs, 3).await {
                    return Ok(false);
                }
            }
            let started = inviter_sessions
                .next_started_with(carol_phone.node_id(), since, Duration::from_millis(500))
                .await?;
            if started.is_none() {
                return Ok::<bool, anyhow::Error>(true);
            }
        }
    })
    .await;
    assert!(
        matches!(quiet, Ok(Ok(true))),
        "pass runs kept reaching the converged tombstone from a member's device, or a member's device from it"
    );

    for node in [alice_phone, bob_phone, carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A member's device minting its tickets to a pod's stores, as it does for
/// every join it serves, dials no former member's device it had sessions
/// with.
///
/// The member's own pass is out of reach, and the act waits for the
/// tombstone to converge and then for a second in which no session with the
/// departed device starts, so only the mint can dial it after.
#[tokio::test(flavor = "multi_thread")]
async fn minting_a_pods_tickets_dials_no_former_members_device() -> Result<()> {
    let (alice_phone, carol_phone) = (node(QUIET).await?, node(POD_RUN).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    let carols = vec![device_of(&carol_phone, &carol)?];
    invite(&alice_phone, &alice, pod, &carol, carols).await?;
    let from_alice = tickets(&alice_phone, &alice, pod).await?;
    carol_phone.import_pod(carol.id, pod, from_alice).await?;
    assert!(lists(&carol_phone, carol.id, pod, carol.id, PLAIN).await?);
    let mut draws = carol_phone
        .take_pod_pass_draws()
        .expect("the draw channel is taken once");

    depart(&carol_phone, &carol, pod, EventKind::Left, &carol, 2).await?;
    carol_phone.forget_pod(carol.id, pod).await?;
    assert!(lists(&alice_phone, alice.id, pod, carol.id, OUT).await?);
    let converged = tokio::time::timeout(TIMEOUT, async {
        loop {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            if draw.store == PodStore::Membership && draw.contacts.is_empty() {
                break;
            }
        }
    })
    .await;
    assert!(converged.is_ok(), "the tombstone did not converge");
    let mut sessions = alice_phone
        .watch_pod_sessions(alice.id, pod, PodStore::Membership)
        .await?;
    let c1 = carol_phone.node_id();
    let quiet = tokio::time::timeout(TIMEOUT, async {
        loop {
            let since = std::time::SystemTime::now();
            if sessions
                .next_started_with(c1, since, Duration::from_secs(1))
                .await?
                .is_none()
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await;
    assert!(
        matches!(quiet, Ok(Ok(()))),
        "sessions with the departed device went on"
    );

    let since = std::time::SystemTime::now();
    alice_phone
        .share_pod_tickets(alice.id, pod, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let reached = sessions
        .next_started_with(c1, since, Duration::from_secs(2))
        .await?;
    assert!(
        reached.is_none(),
        "minting the pod's tickets dialed the former member's device: {reached:?}"
    );

    for node in [alice_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A tombstone whose one remaining member is co-located converges at its
/// first session with that member's replica, inside the process, and is
/// then reconciled with the identity's own devices alone: here, none.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_with_a_co_located_members_replica_converges_a_tombstone() -> Result<()> {
    let tablet = node(POD_RUN).await?;
    let (leisure, _) = host(&tablet).await?;
    let (work, _) = host(&tablet).await?;
    let pod = create(&tablet, &leisure).await?;
    invite(
        &tablet,
        &leisure,
        pod,
        &work,
        vec![device_of(&tablet, &work)?],
    )
    .await?;
    let tickets = tickets(&tablet, &leisure, pod).await?;
    tablet.import_pod(work.id, pod, tickets).await?;
    assert!(lists(&tablet, work.id, pod, work.id, PLAIN).await?);
    let mut draws = tablet
        .take_pod_pass_draws()
        .expect("the draw channel is taken once");

    depart(&tablet, &work, pod, EventKind::Left, &work, 2).await?;
    tablet.forget_pod(work.id, pod).await?;
    assert!(lists(&tablet, leisure.id, pod, work.id, OUT).await?);
    let converged = tokio::time::timeout(TIMEOUT, async {
        loop {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            if draw.identity == work.id
                && draw.store == PodStore::Membership
                && draw.contacts.is_empty()
            {
                break;
            }
        }
    })
    .await;
    assert!(
        converged.is_ok(),
        "the tombstone kept reconciling with its co-located member"
    );

    tablet.shutdown().await?;
    Ok(())
}

/// Whether `count` runs of the pod pass `draws` reports draw a membership
/// store, before [`TIMEOUT`].
async fn membership_runs(
    draws: &mut tokio::sync::mpsc::UnboundedReceiver<data_layer::PodPassDraw>,
    count: usize,
) -> bool {
    let mut left = count;
    tokio::time::timeout(TIMEOUT, async {
        while left > 0 {
            let Some(draw) = draws.recv().await else {
                return false;
            };
            if draw.store == PodStore::Membership {
                left = left.saturating_sub(1);
            }
        }
        true
    })
    .await
    .unwrap_or(false)
}

/// A tombstone that has reached no member's device yet is reconciled with
/// member devices: the flush a leave runs after its departure carries the
/// left event to every one of them.
///
/// Every pass is out of reach and the leaving devices are out of both swarms
/// and settled, so the flush is the left event's one path. The record store
/// is forgotten after the left event is written, as a leave does: a write
/// reaches a pod's membership store only while its record store is held.
#[allow(clippy::too_many_lines)] // one scenario: the leave, the forget and the flush
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_short_of_a_member_flushes_its_departure_to_the_members() -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET).await?, node(QUIET).await?);
    let (carol_phone, carol_laptop) = (node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = linked(&carol_phone, &carol_laptop).await?;
    let pod = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        pod,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let carols = vec![
        device_of(&carol_phone, &carol)?,
        device_of(&carol_laptop, &carol)?,
    ];
    invite(&alice_phone, &alice, pod, &carol, carols).await?;
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for (device, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&carol_laptop, &carol),
    ] {
        device.import_pod(holder.id, pod, tickets.clone()).await?;
        assert!(lists(device, holder.id, pod, bob.id, PLAIN).await?);
    }
    for namespace in [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ] {
        carol_phone
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
        carol_laptop
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
    }
    let phones = [&alice_phone, &bob_phone, &carol_phone, &carol_laptop];
    assert!(settle(&phones, pod).await?);

    depart(&carol_phone, &carol, pod, EventKind::Left, &carol, 2).await?;
    carol_phone.forget_pod(carol.id, pod).await?;
    let flushed = carol_phone
        .flush_pod(carol.id, pod)
        .await?
        .wait(TIMEOUT)
        .await;
    assert!(flushed, "the tombstone's flush reached no member's device");
    for (phone, holder) in [(&alice_phone, &alice), (&bob_phone, &bob)] {
        assert!(
            lists(phone, holder.id, pod, carol.id, OUT).await?,
            "the leave did not reach a member's device"
        );
    }

    for node in [alice_phone, bob_phone, carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Whether a run of the pod pass `draws` reports comes to draw from
/// membership-store contacts that list `contact` as `want` says.
async fn draws_contact(
    draws: &mut tokio::sync::mpsc::UnboundedReceiver<data_layer::PodPassDraw>,
    contact: (data_layer::EndpointId, data_layer::Identity),
    want: bool,
) -> Result<bool> {
    let found = tokio::time::timeout(TIMEOUT, async {
        while let Some(draw) = draws.recv().await {
            let listed = draw
                .contacts
                .iter()
                .any(|drawn| (drawn.addr.id, drawn.identity) == contact);
            if draw.store == PodStore::Membership && listed == want {
                return true;
            }
        }
        false
    })
    .await;
    Ok(found.unwrap_or(false))
}

/// A departed member's device that takes its record store back on a rejoin
/// dials its inviter as the inviter while its membership view still shows
/// the departure. Paired: the converged tombstone, before the rejoin, leaves
/// the inviter out. The inviter writes no joined event, which stands in for
/// one still on its way: any session would otherwise bring it and move the
/// membership view.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoining_device_dials_its_inviter_as_the_inviter_before_its_membership_view_shows_the_join(
) -> Result<()> {
    let (alice_phone, carol_phone) = (node(QUIET).await?, node(POD_RUN).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    let carols = vec![device_of(&carol_phone, &carol)?];
    invite(&alice_phone, &alice, pod, &carol, carols).await?;
    let from_alice = tickets(&alice_phone, &alice, pod).await?;
    carol_phone
        .import_pod(carol.id, pod, from_alice.clone())
        .await?;
    assert!(lists(&carol_phone, carol.id, pod, carol.id, PLAIN).await?);
    let inviter = (
        data_layer::EndpointId::from_bytes(alice_phone.node_id().as_bytes())?,
        identity_of(alice.id),
    );
    let mut draws = carol_phone
        .take_pod_pass_draws()
        .expect("the draw channel is taken once");
    depart(&carol_phone, &carol, pod, EventKind::Left, &carol, 2).await?;
    carol_phone.forget_pod(carol.id, pod).await?;
    assert!(lists(&alice_phone, alice.id, pod, carol.id, OUT).await?);
    // Paired: the tombstone, once a session with the inviter went through.
    assert!(draws_contact(&mut draws, inviter, false).await?);

    carol_phone.import_pod(carol.id, pod, from_alice).await?;
    assert!(
        draws_contact(&mut draws, inviter, true).await?,
        "the rejoining device dropped its inviter from its contacts"
    );
    assert_eq!(state_on(&carol_phone, carol.id, pod, carol.id).await, OUT);

    for node in [alice_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A person hosted on `phone`, its PMS listing `laptop` as a device
/// of its own too, and the laptop joined to it.
async fn linked(phone: &SyncNode, laptop: &SyncNode) -> Result<(Person, PrivateMetadataStore)> {
    let (person, pms) = host(phone).await?;
    pms.add_device(laptop.node_id()).await?;
    let ticket = pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(laptop, person.id, ticket).await?;
    assert!(wait_devices(&laptop_pms, &[phone.node_id(), laptop.node_id()]).await?);
    Ok((person, pms))
}

/// A removed member's record from while a member reads on a device linked
/// after the removal, and once invited again the member writes under its new
/// sequence, both records reading as its own on every device. Denied: its
/// device is refused the record store between the removal and the new join.
#[allow(clippy::too_many_lines)] // one scenario: the removal, the late device and the return
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_member_reads_as_itself_before_and_after_it_joins_again() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop, carol_phone) = (
        node(QUIET).await?,
        node(QUIET).await?,
        node(QUIET).await?,
        node(QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, bob_pms) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let from_alice = tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        phone.import_pod(holder.id, pod, from_alice.clone()).await?;
    }
    // Bob's pull of the claim is served once Carol's phone knows his device.
    let bobs = device_of(&bob_phone, &bob)?;
    assert!(lists_device(&carol_phone, carol.id, pod, bob.id, bobs).await?);
    let earlier = place_claim(&carol_phone, &carol, pod, 1).await?;
    assert!(reads(&bob_phone, bob.id, pod, earlier).await?);

    depart(&alice_phone, &carol, pod, EventKind::Removed, &alice, 2).await?;
    assert!(lists(&bob_phone, bob.id, pod, carol.id, OUT).await?);
    assert!(lists(&carol_phone, carol.id, pod, carol.id, OUT).await?);
    // Denied: Carol's device, on the record store, while removed.
    let mut carols = carol_phone
        .watch_pod_sessions(carol.id, pod, PodStore::Records)
        .await?;
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = carols.next_with(bob_phone.node_id(), true, TIMEOUT).await?;
    assert!(refused.is_some_and(|session| session.exchanged.is_err()));

    // Bob's laptop, linked after the removal, reads Carol's earlier record.
    bob_pms.add_device(bob_laptop.node_id()).await?;
    let pms_ticket = bob_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    join_identity(&bob_laptop, bob.id, pms_ticket).await?;
    let bobs = MembershipKey::Devices {
        member: bob.id,
        version: 2,
    };
    let statement = bob
        .keys
        .device_statement(
            &pod,
            2,
            vec![device_of(&bob_phone, &bob)?, device_of(&bob_laptop, &bob)?],
        )
        .encode();
    write(&bob_phone, &bob, pod, bobs, statement).await?;
    bob_laptop
        .import_pod(bob.id, pod, tickets(&bob_phone, &bob, pod).await?)
        .await?;
    assert!(
        reads(&bob_laptop, bob.id, pod, earlier).await?,
        "a departed member's earlier record did not read on a device linked after"
    );

    // Invited again, at Carol's sequence 3, Carol writes under it.
    let rejoined = MembershipKey::Event {
        subject: carol.id,
        seq: Seq::new(3),
        kind: EventKind::Joined,
        actor: alice.id,
        actor_seq: Seq::FIRST,
    };
    let join_statement = carol
        .keys
        .join_statement(&pod, Seq::new(3), device_of(&carol_phone, &carol)?)
        .encode();
    write(&alice_phone, &alice, pod, rejoined, join_statement).await?;
    dial(
        &carol_phone,
        &carol,
        pod,
        PodStore::Membership,
        &alice_phone,
        &alice,
    )
    .await?;
    assert!(lists(&carol_phone, carol.id, pod, carol.id, PLAIN).await?);
    let later = data_layer::RecordKey::Claim {
        member: carol.id,
        id: pdn_types::RecordId::from_bytes([3; 16]),
        mseq: Seq::new(3),
    };
    carol_phone
        .write_pod_entry(
            carol.id,
            pod,
            PodStore::Records,
            &later.to_bytes(),
            b"claim",
        )
        .await?;
    for (device, holder) in [(&bob_phone, &bob), (&alice_phone, &alice)] {
        dial(&carol_phone, &carol, pod, PodStore::Records, device, holder).await?;
        assert!(
            reads(device, holder.id, pod, later.record()).await?,
            "the record written under the new sequence did not read"
        );
        assert!(reads(device, holder.id, pod, earlier).await?);
    }

    for node in [alice_phone, bob_phone, bob_laptop, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A leave dated before the join it ends, as a device whose clock runs
/// behind the one that recorded the join dates it, still ends the
/// identity's holding of the pod on each of its devices, and a join at a
/// later sequence holds the pod again. The tombstone is written before the
/// join's record, so its date is the earlier of the two.
#[tokio::test(flavor = "multi_thread")]
async fn a_leave_dated_before_its_join_still_ends_holding_on_every_device() -> Result<()> {
    let (carol_phone, carol_laptop) = (node(QUIET).await?, node(QUIET).await?);
    let (carol, phone_pms) = host(&carol_phone).await?;
    phone_pms.add_device(carol_laptop.node_id()).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(&carol_laptop, carol.id, ticket).await?;
    let both = [carol_phone.node_id(), carol_laptop.node_id()];
    assert!(wait_devices(&laptop_pms, &both).await?);
    let pod = PodId::from_bytes([0x9c; 16]);

    laptop_pms.tombstone_pod(pod, Seq::new(2)).await?;
    phone_pms.record_pod(pod, Seq::FIRST).await?;
    for pms in [&phone_pms, &laptop_pms] {
        assert!(
            test_utils::eventually(|| async {
                Ok(pms.held_pods().await?.is_empty() && pms.departed_pods().await? == [pod])
            })
            .await?,
            "the leave lost to a join dated after it"
        );
    }
    phone_pms.record_pod(pod, Seq::new(3)).await?;
    for pms in [&phone_pms, &laptop_pms] {
        assert!(
            test_utils::eventually(|| async { Ok(pms.held_pods().await? == [pod]) }).await?,
            "the join after the leave did not hold the pod again"
        );
    }

    for node in [carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}
