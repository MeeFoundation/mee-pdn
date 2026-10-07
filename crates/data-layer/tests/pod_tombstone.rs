//! A departed member's devices and the pod's membership store they keep as
//! its tombstone: served by member devices over the departure's past alone,
//! both ways, refused the record store, served whole by a sibling, and
//! reconciled with member devices until one session with them goes
//! through, then with the identity's own devices alone. The entries a
//! pod's creation, its joins and its departures write arrive by the
//! store-level writes the pods service performs, and the tickets by hand.

use std::{collections::HashSet, time::Duration};

use anyhow::Result;
use data_layer::{
    identity_of, AddrInfoOptions, AuthorId, Contact, EventKind, MemberDevice, MemberState,
    MembershipKey, PodStore, PrivateMetadataStore, Seq, ShareMode, SpawnOptions, SyncNode,
};
use pdn_types::PodId;
use test_utils::{
    join_identity,
    pod::{
        create, device_of, holds_no_record, host, invite, lists, lists_device, place_claim, reads,
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
        .pod_membership(holder.id, pod)
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

/// A device offline while its member is removed learns of the removal at its
/// first session with a member device, taking the removal and what it rests
/// on. Denied: a membership event outside the removal's past and a record
/// placed after it reach the device from no member device, the record
/// store refused to it.
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
    let dave = Person::generate();
    invite(&bob_phone, &bob, pod, &dave, vec![nowhere(0xd0)]).await?;
    let claim = place_claim(&alice_phone, &alice, pod, 1).await?;
    assert!(lists(&bob_phone, bob.id, pod, carol.id, OUT).await?);
    assert!(lists(&alice_phone, alice.id, pod, dave.id, PLAIN).await?);
    assert!(reads(&bob_phone, bob.id, pod, claim).await?);

    let carol_phone = node_on(dir.path()).await?;
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
    assert!(holds_no_record(&carol_phone, carol.id, pod).await?);

    for node in [alice_phone, bob_phone, carol_phone] {
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
/// identity's own devices alone.
#[allow(clippy::too_many_lines)] // one scenario: the leave and the contacts before and after
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_is_reconciled_with_its_siblings_alone_once_it_reached_a_member() -> Result<()>
{
    let (alice_phone, bob_phone) = (node(QUIET).await?, node(QUIET).await?);
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

    for node in [alice_phone, bob_phone, carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
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
/// dials its inviter as the inviter while its fold still shows the
/// departure. Paired: the converged tombstone, before the rejoin, leaves the
/// inviter out. The inviter writes no joined event, which stands in for one
/// still on its way: any session would otherwise bring it and move the fold.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoining_device_dials_its_inviter_as_the_inviter_before_its_fold_shows_the_join(
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

/// A person hosted on `phone`, its directory listing `laptop` as a device
/// of its own too, and the laptop joined to it.
async fn linked(phone: &SyncNode, laptop: &SyncNode) -> Result<(Person, PrivateMetadataStore)> {
    let (person, directory) = host(phone).await?;
    directory.add_device(laptop.node_id()).await?;
    let ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_directory = join_identity(laptop, person.id, ticket).await?;
    assert!(wait_devices(&laptop_directory, &[phone.node_id(), laptop.node_id()]).await?);
    Ok((person, directory))
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
    let (bob, bob_directory) = host(&bob_phone).await?;
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
    bob_directory.add_device(bob_laptop.node_id()).await?;
    let directory_ticket = bob_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    join_identity(&bob_laptop, bob.id, directory_ticket).await?;
    let bobs = MembershipKey::Devices {
        member: bob.id,
        version: 2,
    };
    let statement = bob
        .keys
        .device_statement(
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
    let join_statement = carol.keys.join_statement(&pod, Seq::new(3)).encode();
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
    let (carol, phone_directory) = host(&carol_phone).await?;
    phone_directory.add_device(carol_laptop.node_id()).await?;
    let ticket = phone_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_directory = join_identity(&carol_laptop, carol.id, ticket).await?;
    let both = [carol_phone.node_id(), carol_laptop.node_id()];
    assert!(wait_devices(&laptop_directory, &both).await?);
    let pod = PodId::from_bytes([0x9c; 16]);

    laptop_directory.tombstone_pod(pod, Seq::new(2)).await?;
    phone_directory.record_pod(pod, Seq::FIRST).await?;
    for directory in [&phone_directory, &laptop_directory] {
        assert!(
            test_utils::eventually(|| async {
                Ok(directory.held_pods().await?.is_empty()
                    && directory.departed_pods().await? == [pod])
            })
            .await?,
            "the leave lost to a join dated after it"
        );
    }
    phone_directory.record_pod(pod, Seq::new(3)).await?;
    for directory in [&phone_directory, &laptop_directory] {
        assert!(
            test_utils::eventually(|| async { Ok(directory.held_pods().await? == [pod]) }).await?,
            "the join after the leave did not hold the pod again"
        );
    }

    for node in [carol_phone, carol_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}
