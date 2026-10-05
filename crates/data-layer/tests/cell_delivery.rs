//! How a write to a cell's store reaches the member devices — through the
//! store's swarm, a content-free announcement and the pull it triggers, and
//! through the cell stores' own pass, whose every run reconciles with at
//! most 5 peers drawn from contacts the membership derives — and the order
//! the two stores reconcile in, the record store after the membership
//! store with the same counterpart. The entries a cell's creation and its
//! joins write arrive by the store-level writes the cells service performs,
//! and the tickets by hand.

use std::{collections::HashSet, time::Duration};

use anyhow::Result;
use data_layer::{
    cell_id_of, identity_of, AuthorId, CellStore, Contact, EventKind, MemberDevice, MemberState,
    MembershipKey, Seq, SpawnOptions, SyncNode,
};
use pdn_types::{CellId, NodeId};
use test_utils::{
    cell::{
        device_of, folds_nobody, found, holds_no_record, host, invite, lists, lists_device,
        place_claim, reads, state_on, statement, tickets, write, Person,
    },
    eventually, TIMEOUT,
};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);
/// A cell pass short enough that a scenario waits a few runs at most.
const CELL_RUN: Duration = Duration::from_millis(300);

const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};
const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};

async fn node(reconcile_interval: Duration, cell_reconcile_interval: Duration) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval,
        cell_reconcile_interval,
        ..SpawnOptions::memory()
    })
    .await
}

/// A device no node runs: a statement may list it, and nothing answers a
/// dial to it.
fn nowhere(seed: u8) -> MemberDevice {
    let key = iroh::SecretKey::from_bytes(&[seed; 32]).public();
    MemberDevice {
        node: NodeId::from_bytes(*key.as_bytes()),
        author: AuthorId::from([seed; 32]),
    }
}

/// A dial of one of `cell`'s stores from `holder`'s replica on `from` to
/// `callee`'s on `to`, as a drawn contact is dialed.
async fn dial(
    from: &SyncNode,
    holder: &Person,
    cell: CellId,
    store: CellStore,
    to: &SyncNode,
    callee: &Person,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee.id));
    from.sync_cell_with_for_test(holder.id, cell, store, contact)
        .await
}

/// Whether `cell`'s stores come to have nothing in flight on every one of
/// `nodes` — no exchange running, held or due to redial — in two reads in
/// a row: a dial one node still makes lands on another between two reads.
/// Out of a swarm, a store is still dialed by every node it had a session
/// with until then.
async fn settle(nodes: &[&SyncNode], cell: CellId) -> Result<bool> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut quiet_reads = 0_u8;
    while std::time::Instant::now() < deadline {
        let mut in_flight = 0_usize;
        for node in nodes {
            in_flight = in_flight.saturating_add(node.cell_syncs_in_flight_for_test(cell).await?);
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

/// A write reaches every subscribed member device through the store's swarm
/// alone, on either store. Denied: a holder of both tickets that is no
/// member, in the same swarms, takes nothing.
///
/// Both passes are out of reach, so after the catch-up an entry arrives
/// through the swarm alone: the first write by its announcement or by the
/// session a neighbour's arrival opens, the second, once that has happened,
/// by its announcement.
#[allow(clippy::too_many_lines)] // one scenario: two live writes beside the denial
#[tokio::test(flavor = "multi_thread")]
async fn a_write_arrives_live_over_the_swarm_and_a_ticket_holder_takes_nothing() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &carol,
        vec![device_of(&carol_phone, &carol)?],
    )
    .await?;
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    for (phone, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&dave_phone, &dave),
    ] {
        phone.import_cell(holder.id, cell, tickets.clone()).await?;
    }
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        assert!(
            lists(phone, holder.id, cell, alice.id, OWNER).await?,
            "a member's device did not catch up from its first session"
        );
    }

    for _write in 0..2 {
        let newcomer = Person::generate();
        invite(&alice_phone, &alice, cell, &newcomer, vec![nowhere(0xe0)]).await?;
        for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
            assert!(
                lists(phone, holder.id, cell, newcomer.id, PLAIN).await?,
                "a write did not reach a member's device over the swarm"
            );
        }
    }
    let claim = place_claim(&alice_phone, &alice, cell, 1).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        assert!(
            reads(phone, holder.id, cell, claim).await?,
            "a record did not reach a member's device over the swarm"
        );
    }
    // Denied: the ticket holder that is no member, on either store.
    assert!(
        folds_nobody(&dave_phone, dave.id, cell).await?,
        "the ticket holder took entries from the swarm"
    );
    assert!(holds_no_record(&dave_phone, dave.id, cell).await?);

    for node in [alice_phone, bob_phone, carol_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A write reaches a member device that never synced with its author
/// through another member's device, payload included, while the author's
/// device is offline. Denied: a holder of that member's tickets that is no
/// member takes nothing from it.
///
/// A joined event counts once the join statement in its payload verifies,
/// so the newcomer listed on Carol's phone proves the payload came from
/// Bob's.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_reaches_a_member_through_another_member_payload_included() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &carol,
        vec![device_of(&carol_phone, &carol)?],
    )
    .await?;
    bob_phone
        .import_cell(bob.id, cell, tickets(&alice_phone, &alice, cell).await?)
        .await?;
    let erin = Person::generate();
    invite(&alice_phone, &alice, cell, &erin, vec![nowhere(0xe0)]).await?;
    // Everything Carol's phone needs, payloads included, is on Bob's phone
    // before Alice's goes.
    for member in [&carol, &erin] {
        assert!(lists(&bob_phone, bob.id, cell, member.id, PLAIN).await?);
    }
    alice_phone.shutdown().await?;

    let from_bob = tickets(&bob_phone, &bob, cell).await?;
    carol_phone
        .import_cell(carol.id, cell, from_bob.clone())
        .await?;
    dave_phone.import_cell(dave.id, cell, from_bob).await?;
    assert!(
        lists(&carol_phone, carol.id, cell, erin.id, PLAIN).await?,
        "the write did not reach Carol's phone through Bob's"
    );
    // Denied: the ticket holder that is no member.
    assert!(folds_nobody(&dave_phone, dave.id, cell).await?);

    for node in [bob_phone, carol_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Each run of the cell pass reaches at most 5 peers per store, drawn from
/// contacts the membership derives: every device of every current member
/// dialed as that member, a co-located member's device among them and
/// reached inside the process, and the identity's own siblings by its
/// directory, never this device as itself. Denied: once a member is kicked,
/// no run's contacts hold its devices.
///
/// Nothing is written between the co-located member's catch-up and its
/// in-process sessions being counted, and the other pass is out of reach,
/// so every session counted comes from a draw.
#[allow(clippy::too_many_lines)] // one scenario: the derivation, the draws and the kick
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_pass_run_reaches_at_most_five_peers_of_the_contacts_the_membership_derives(
) -> Result<()> {
    let tablet = node(QUIET, CELL_RUN).await?;
    let (alice, alice_directory) = host(&tablet).await?;
    let (carol, _) = host(&tablet).await?;
    let sibling = nowhere(0xa2);
    alice_directory.add_device(sibling.node).await?;
    let cell = found(&tablet, &alice).await?;
    let bob = Person::generate();
    let bobs: Vec<MemberDevice> = (0xb1..=0xb7).map(nowhere).collect();
    invite(&tablet, &alice, cell, &bob, bobs.clone()).await?;
    invite(
        &tablet,
        &alice,
        cell,
        &carol,
        vec![device_of(&tablet, &carol)?],
    )
    .await?;
    let mut draws = tablet
        .take_cell_pass_draws()
        .expect("the draw channel is taken once");
    tablet
        .import_cell(carol.id, cell, tickets(&tablet, &alice, cell).await?)
        .await?;
    assert!(lists(&tablet, carol.id, cell, alice.id, OWNER).await?);
    let served_before = tablet.in_process_sessions_served(carol.id, alice.id)?;

    let endpoint = |device: &MemberDevice| iroh::EndpointId::from_bytes(device.node.as_bytes());
    let mut expected: HashSet<(iroh::EndpointId, data_layer::Identity)> = HashSet::new();
    for device in &bobs {
        expected.insert((endpoint(device)?, identity_of(bob.id)));
    }
    expected.insert((
        endpoint(&device_of(&tablet, &carol)?)?,
        identity_of(carol.id),
    ));
    expected.insert((endpoint(&sibling)?, identity_of(alice.id)));
    let mut stores_drawn = HashSet::new();
    let mut reached = HashSet::new();
    // A run whose derivation began before the last invite can report after
    // the channel was taken, without it.
    let mut derived = false;
    tokio::time::timeout(TIMEOUT, async {
        while stores_drawn.len() < 2 || reached.len() <= 5 {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            if draw.identity != alice.id || draw.cell != cell {
                continue;
            }
            let contacts: HashSet<_> = draw
                .contacts
                .iter()
                .map(|contact| (contact.addr.id, contact.identity))
                .collect();
            derived |= contacts == expected;
            if !derived {
                continue;
            }
            assert_eq!(contacts, expected, "the contacts the membership derives");
            assert!(draw.drawn.len() + draw.recorded.len() <= 5);
            for contact in &draw.drawn {
                assert!(contacts.contains(&(contact.addr.id, contact.identity)));
                reached.insert((contact.addr.id, contact.identity));
            }
            stores_drawn.insert(draw.store);
        }
    })
    .await?;

    assert!(
        eventually(|| async {
            Ok(tablet.in_process_sessions_served(carol.id, alice.id)? > served_before)
        })
        .await?,
        "a drawn contact naming this node was not reached inside the process"
    );

    // Denied: Bob's devices, once Alice kicks him.
    let kick = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(2),
        kind: EventKind::Kicked,
        actor: alice.id,
        actor_seq: Seq::FIRST,
    };
    write(&tablet, &alice, cell, kick, vec![0]).await?;
    let without_bob: HashSet<_> = expected
        .iter()
        .filter(|(_device, identity)| *identity != identity_of(bob.id))
        .copied()
        .collect();
    let mut stores_replaced = HashSet::new();
    tokio::time::timeout(TIMEOUT, async {
        while stores_replaced.len() < 2 {
            let draw = draws.recv().await.expect("the pass stopped drawing");
            let contacts: HashSet<_> = draw
                .contacts
                .iter()
                .map(|contact| (contact.addr.id, contact.identity))
                .collect();
            if draw.identity == alice.id && contacts == without_bob {
                stores_replaced.insert(draw.store);
            }
        }
    })
    .await?;

    tablet.shutdown().await?;
    Ok(())
}

/// The device an invite's statement lists is the inviter's contact on both
/// stores, as the newcomer, once the writes return, before any newcomer can
/// write. Paired: before the invite it is no contact.
#[tokio::test(flavor = "multi_thread")]
async fn an_invited_device_is_a_contact_once_the_invite_is_written() -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET, QUIET).await?, node(QUIET, QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    let as_bob = Contact::new(bob_phone.dial_handle().addr(), identity_of(bob.id));
    let lists_bob = |contacts: Vec<Contact>| {
        contacts
            .iter()
            .any(|contact| contact.addr.id == as_bob.addr.id && contact.identity == as_bob.identity)
    };
    for store in [CellStore::Membership, CellStore::Records] {
        // Paired: before the invite.
        let before = alice_phone.cell_contacts_for_test(alice.id, cell, store)?;
        assert!(!lists_bob(before));
    }

    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    for store in [CellStore::Membership, CellStore::Records] {
        let after = alice_phone.cell_contacts_for_test(alice.id, cell, store)?;
        assert!(
            lists_bob(after),
            "{store:?}: the invited device is no contact yet"
        );
    }

    for node in [alice_phone, bob_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A device another member's invite lists is a contact, as the newcomer,
/// once the session that brings the invite ends, with no quiet wait before
/// it: the newcomer's first announcement can come at once. Paired: before
/// that session it is no contact. The change watch's quiet wait runs an hour
/// here, so only the derivation an arriving entry starts can list it.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_an_arriving_invite_lists_is_a_contact_with_no_quiet_wait() -> Result<()> {
    let spawn = || {
        SyncNode::spawn(SpawnOptions {
            reconcile_interval: QUIET,
            cell_reconcile_interval: QUIET,
            cell_change_settle: QUIET,
            ..SpawnOptions::memory()
        })
    };
    let (alice_phone, carol_phone) = (spawn().await?, spawn().await?);
    let (alice, _) = host(&alice_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &carol,
        vec![device_of(&carol_phone, &carol)?],
    )
    .await?;
    carol_phone
        .import_cell(carol.id, cell, tickets(&alice_phone, &alice, cell).await?)
        .await?;
    assert!(lists(&carol_phone, carol.id, cell, carol.id, PLAIN).await?);
    let dave = Person::generate();
    let daves = nowhere(0xd0);
    let lists_dave = |contacts: Vec<Contact>| {
        contacts.iter().any(|contact| {
            contact.addr.id.as_bytes() == daves.node.as_bytes()
                && contact.identity == identity_of(dave.id)
        })
    };
    // Paired: before the session that brings the invite.
    let before = carol_phone.cell_contacts_for_test(carol.id, cell, CellStore::Membership)?;
    assert!(!lists_dave(before));

    invite(&alice_phone, &alice, cell, &dave, vec![daves]).await?;
    dial(
        &carol_phone,
        &carol,
        cell,
        CellStore::Membership,
        &alice_phone,
        &alice,
    )
    .await?;
    assert!(lists_device(&carol_phone, carol.id, cell, dave.id, daves).await?);
    assert!(
        eventually(|| async {
            let contacts =
                carol_phone.cell_contacts_for_test(carol.id, cell, CellStore::Membership)?;
            Ok(lists_dave(contacts))
        })
        .await?,
        "the device an arriving invite lists waited for a quiet store"
    );

    for node in [alice_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A newcomer's record store reaches the inviter by its ticket, and the
/// inviter's claim reads, though the membership store's first session
/// derives contacts that leave the inviter's device out. Alice's statement
/// lists another node here, as a fold whose payloads are still on their way
/// lists none; the start is held between the two stores until that
/// derivation lands, which the stress pass meets only now and then.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_store_starts_with_its_ticket_though_the_first_session_lists_no_inviter_device(
) -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET, QUIET).await?, node(QUIET, QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let founding = alice.keys.founding([0xa0; 16]);
    let cell = cell_id_of(&alice.id, &founding.announcement_key, &founding.nonce);
    alice_phone.create_cell(alice.id, cell).await?;
    write(
        &alice_phone,
        &alice,
        cell,
        MembershipKey::founded(alice.id),
        founding.encode(),
    )
    .await?;
    let elsewhere = MemberDevice {
        node: nowhere(0xa0).node,
        author: alice_phone.default_author(alice.id)?,
    };
    statement(&alice_phone, &alice, cell, &alice, vec![elsewhere]).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let claim = place_claim(&alice_phone, &alice, cell, 1).await?;
    let tickets = tickets(&alice_phone, &alice, cell).await?;

    let pause = bob_phone.pause_next_cell_start_for_test();
    let only_elsewhere = |contacts: Vec<Contact>| {
        contacts.len() == 1
            && contacts
                .iter()
                .all(|contact| contact.addr.id.as_bytes() == elsewhere.node.as_bytes())
    };
    let (imported, derived) = tokio::join!(bob_phone.import_cell(bob.id, cell, tickets), async {
        pause.reached.notified().await;
        let derived = eventually(|| async {
            let contacts = bob_phone.cell_contacts_for_test(bob.id, cell, CellStore::Records)?;
            Ok(only_elsewhere(contacts))
        })
        .await;
        pause.release.notify_one();
        derived
    });
    imported?;
    assert!(
        derived?,
        "the membership store's first session derived no contacts"
    );
    assert!(
        reads(&bob_phone, bob.id, cell, claim).await?,
        "the record store started with the contacts derived after its membership store's start"
    );

    for node in [alice_phone, bob_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// The inviter's write reaches a newcomer live after the newcomer shared its
/// own tickets, while the contacts its membership derives still leave the
/// inviter's device out: the pull its announcement triggers addresses the
/// inviter, whose ticket the stores came from. Alice's statement lists
/// another node here, as a fold whose payloads are still on their way lists
/// none.
#[tokio::test(flavor = "multi_thread")]
async fn a_newcomer_that_shared_its_tickets_pulls_from_an_inviter_its_contacts_leave_out(
) -> Result<()> {
    let (alice_phone, bob_phone) = (node(QUIET, QUIET).await?, node(QUIET, QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let founding = alice.keys.founding([0xa1; 16]);
    let cell = cell_id_of(&alice.id, &founding.announcement_key, &founding.nonce);
    alice_phone.create_cell(alice.id, cell).await?;
    write(
        &alice_phone,
        &alice,
        cell,
        MembershipKey::founded(alice.id),
        founding.encode(),
    )
    .await?;
    let elsewhere = MemberDevice {
        node: nowhere(0xa1).node,
        author: alice_phone.default_author(alice.id)?,
    };
    statement(&alice_phone, &alice, cell, &alice, vec![elsewhere]).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    bob_phone
        .import_cell(bob.id, cell, tickets(&alice_phone, &alice, cell).await?)
        .await?;
    assert!(lists(&bob_phone, bob.id, cell, bob.id, PLAIN).await?);
    assert!(
        eventually(|| async {
            let contacts = bob_phone.cell_contacts_for_test(bob.id, cell, CellStore::Records)?;
            Ok(contacts.len() == 1
                && contacts
                    .iter()
                    .all(|contact| contact.addr.id.as_bytes() == elsewhere.node.as_bytes()))
        })
        .await?,
        "the contacts the membership derives still name the inviter's device"
    );
    tickets(&bob_phone, &bob, cell).await?;

    let claim = place_claim(&alice_phone, &alice, cell, 1).await?;
    assert!(
        reads(&bob_phone, bob.id, cell, claim).await?,
        "the pull the announcement triggered addressed another identity than the inviter"
    );

    for node in [alice_phone, bob_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A write whose announcement a member device missed reaches it at the next
/// run of its cell pass. Denied: a holder of both tickets that is no member,
/// running the same pass, takes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_whose_announcement_was_lost_arrives_at_the_next_run() -> Result<()> {
    let (alice_phone, bob_phone, dave_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, CELL_RUN).await?,
        node(QUIET, CELL_RUN).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    let membership = tickets.membership.capability.id();
    bob_phone.import_cell(bob.id, cell, tickets.clone()).await?;
    dave_phone.import_cell(dave.id, cell, tickets).await?;
    assert!(lists(&bob_phone, bob.id, cell, alice.id, OWNER).await?);
    // Out of the swarm, so the announcement of Erin's invite is lost to
    // Bob's phone.
    bob_phone.leave_swarm_for_test(bob.id, membership).await?;

    let erin = Person::generate();
    invite(&alice_phone, &alice, cell, &erin, vec![nowhere(0xe0)]).await?;
    assert!(
        lists(&bob_phone, bob.id, cell, erin.id, PLAIN).await?,
        "the cell pass did not bring the write whose announcement was lost"
    );
    // Denied: the ticket holder that is no member.
    assert!(folds_nobody(&dave_phone, dave.id, cell).await?);

    for node in [alice_phone, bob_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// The pass over every other store leaves a cell's stores to their own
/// interval: a write whose announcement a member device missed stays
/// missing through runs of that pass while the cell pass is out of reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_store_keeps_its_own_interval() -> Result<()> {
    let (alice_phone, bob_phone) = (
        node(QUIET, QUIET).await?,
        node(Duration::from_millis(100), QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    let membership = tickets.membership.capability.id();
    bob_phone.import_cell(bob.id, cell, tickets).await?;
    assert!(lists(&bob_phone, bob.id, cell, alice.id, OWNER).await?);
    bob_phone.leave_swarm_for_test(bob.id, membership).await?;
    // Settled, so no dial the import left behind carries the write.
    assert!(settle(&[&alice_phone, &bob_phone], cell).await?);

    let erin = Person::generate();
    invite(&alice_phone, &alice, cell, &erin, vec![nowhere(0xe0)]).await?;
    // Sentinel: ten runs of the other pass after the write.
    let after_write = bob_phone.reconcile_passes();
    assert!(eventually(|| async { Ok(bob_phone.reconcile_passes() >= after_write + 10) }).await?);
    assert_eq!(
        state_on(&bob_phone, bob.id, cell, erin.id).await,
        MemberState::default(),
        "the pass over other stores reconciled a cell store"
    );

    for node in [alice_phone, bob_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A member device that lacks a newcomer's joined event and first record
/// reads the record at the end of the pair of sessions it dials with a
/// member device holding both, the membership store's first. Denied: a
/// holder of both tickets that is no member, dialing the same way, is
/// refused and takes nothing.
#[allow(clippy::too_many_lines)] // one scenario: the newcomer's entries, the pair of sessions and the denial
#[tokio::test(flavor = "multi_thread")]
async fn a_newcomers_first_record_reads_once_the_session_brings_its_membership() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone, erin_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (erin, _) = host(&erin_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        invite(
            &alice_phone,
            &alice,
            cell,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    for (phone, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&erin_phone, &erin),
    ] {
        phone.import_cell(holder.id, cell, tickets.clone()).await?;
    }
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        assert!(lists(phone, holder.id, cell, alice.id, OWNER).await?);
    }
    // Out of both swarms and settled, so Dave's entries reach Carol's phone
    // only by the sessions she dials.
    for namespace in [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ] {
        carol_phone
            .leave_swarm_for_test(carol.id, namespace)
            .await?;
    }
    assert!(settle(&[&alice_phone, &bob_phone, &carol_phone, &erin_phone], cell).await?);

    invite(
        &alice_phone,
        &alice,
        cell,
        &dave,
        vec![device_of(&dave_phone, &dave)?],
    )
    .await?;
    dave_phone.import_cell(dave.id, cell, tickets).await?;
    // Bob's pull of the claim is served once Dave's phone knows his device.
    let bobs = device_of(&bob_phone, &bob)?;
    assert!(lists_device(&dave_phone, dave.id, cell, bob.id, bobs).await?);
    let claim = place_claim(&dave_phone, &dave, cell, 4).await?;
    assert!(reads(&bob_phone, bob.id, cell, claim).await?);

    dial(
        &carol_phone,
        &carol,
        cell,
        CellStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    assert!(
        reads(&carol_phone, carol.id, cell, claim).await?,
        "the newcomer's record did not read after the pair of sessions"
    );
    // Denied: the ticket holder that is no member.
    let mut erins = erin_phone
        .watch_cell_sessions(erin.id, cell, CellStore::Records)
        .await?;
    dial(
        &erin_phone,
        &erin,
        cell,
        CellStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = erins.next_with(bob_phone.node_id(), true, TIMEOUT).await?;
    assert!(refused.is_some_and(|session| session.exchanged.is_err()));
    assert!(holds_no_record(&erin_phone, erin.id, cell).await?);

    for node in [alice_phone, bob_phone, carol_phone, dave_phone, erin_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A newcomer is served the record store by a member device in the session
/// after the one that brings the device its joined event. Denied: the
/// newcomer's own dial to that device while the device does not know it.
#[allow(clippy::too_many_lines)] // one scenario: the refusal and the pair of sessions that ends it
#[tokio::test(flavor = "multi_thread")]
async fn a_newcomer_is_served_the_record_store_in_the_session_after_the_one_that_brings_its_joined_event(
) -> Result<()> {
    let (alice_phone, bob_phone, nina_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (nina, _) = host(&nina_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        cell,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    bob_phone.import_cell(bob.id, cell, tickets.clone()).await?;
    assert!(lists(&bob_phone, bob.id, cell, alice.id, OWNER).await?);
    // Out of both swarms and settled, so Nina's joined event and Bob's claim
    // cross only in the sessions dialed below.
    for namespace in [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ] {
        bob_phone.leave_swarm_for_test(bob.id, namespace).await?;
    }
    assert!(settle(&[&alice_phone, &bob_phone], cell).await?);
    let claim = place_claim(&bob_phone, &bob, cell, 2).await?;

    invite(
        &alice_phone,
        &alice,
        cell,
        &nina,
        vec![device_of(&nina_phone, &nina)?],
    )
    .await?;
    nina_phone.import_cell(nina.id, cell, tickets).await?;
    let bobs = device_of(&bob_phone, &bob)?;
    assert!(lists_device(&nina_phone, nina.id, cell, bob.id, bobs).await?);
    let mut sessions = nina_phone
        .watch_cell_sessions(nina.id, cell, CellStore::Records)
        .await?;

    // Denied: Nina's own dial, to a device her joined event has not reached.
    dial(
        &nina_phone,
        &nina,
        cell,
        CellStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = sessions
        .next_with(bob_phone.node_id(), true, TIMEOUT)
        .await?;
    assert!(
        refused.is_some_and(|session| session.exchanged.is_err()),
        "a device that does not know the newcomer served it"
    );

    dial(
        &bob_phone,
        &bob,
        cell,
        CellStore::Records,
        &nina_phone,
        &nina,
    )
    .await?;
    let served = sessions
        .next_served_with(bob_phone.node_id(), TIMEOUT)
        .await?;
    assert!(
        served
            .as_ref()
            .is_some_and(|session| session.exchanged.is_ok()),
        "the session after the one that brought the joined event refused the newcomer: {served:?}"
    );
    assert!(reads(&nina_phone, nina.id, cell, claim).await?);

    for node in [alice_phone, bob_phone, nina_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A kicked member, served the record store before its kick, is refused it
/// by a member device in the session after the one that brings the device
/// the kick.
#[allow(clippy::too_many_lines)] // one scenario: the served session, the kick and the refusal
#[tokio::test(flavor = "multi_thread")]
async fn a_kicked_member_is_refused_the_record_store_in_the_session_after_the_one_that_brings_its_kick(
) -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
        node(QUIET, QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let cell = found(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        invite(
            &alice_phone,
            &alice,
            cell,
            member,
            vec![device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = tickets(&alice_phone, &alice, cell).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        phone.import_cell(holder.id, cell, tickets.clone()).await?;
        assert!(lists(phone, holder.id, cell, alice.id, OWNER).await?);
    }
    let carols = device_of(&carol_phone, &carol)?;
    assert!(lists_device(&bob_phone, bob.id, cell, carol.id, carols).await?);
    let phones = [&alice_phone, &bob_phone, &carol_phone];
    // Settled, so the session read below is the one Carol's phone dials.
    assert!(settle(&phones, cell).await?);
    let mut sessions = carol_phone
        .watch_cell_sessions(carol.id, cell, CellStore::Records)
        .await?;
    dial(
        &carol_phone,
        &carol,
        cell,
        CellStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let served = sessions
        .next_with(bob_phone.node_id(), true, TIMEOUT)
        .await?;
    assert!(
        served
            .as_ref()
            .is_some_and(|session| session.exchanged.is_ok()),
        "a member device refused a member: {served:?}"
    );
    // Out of the membership store's swarm and settled, so the kick reaches
    // Bob's phone only in the session Carol's phone dials.
    bob_phone
        .leave_swarm_for_test(bob.id, tickets.membership.capability.id())
        .await?;
    assert!(settle(&phones, cell).await?);

    let kick = MembershipKey::Event {
        subject: carol.id,
        seq: Seq::new(2),
        kind: EventKind::Kicked,
        actor: alice.id,
        actor_seq: Seq::FIRST,
    };
    write(&alice_phone, &alice, cell, kick, vec![0]).await?;
    assert!(
        lists(
            &carol_phone,
            carol.id,
            cell,
            carol.id,
            MemberState::default()
        )
        .await?
    );
    assert_eq!(state_on(&bob_phone, bob.id, cell, carol.id).await, PLAIN);

    let mut sessions = carol_phone
        .watch_cell_sessions(carol.id, cell, CellStore::Records)
        .await?;
    dial(
        &carol_phone,
        &carol,
        cell,
        CellStore::Records,
        &bob_phone,
        &bob,
    )
    .await?;
    let refused = sessions
        .next_with(bob_phone.node_id(), true, TIMEOUT)
        .await?;
    assert!(
        refused.is_some_and(|session| session.exchanged.is_err()),
        "the session after the one that brought the kick served the kicked member"
    );
    assert_eq!(
        state_on(&bob_phone, bob.id, cell, carol.id).await,
        MemberState::default()
    );

    for node in [alice_phone, bob_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A member's write reaches a co-located newcomer it does not yet know
/// through the announcement inside the process, which reconciles the
/// membership store first. Denied: a co-located holder of both tickets
/// that is no member takes nothing from the same announcement.
///
/// The writer and the remote member are out of both swarms and every pass
/// out of reach, so the announcement inside the process is the claim's one
/// path.
#[allow(clippy::too_many_lines)] // one scenario: two joins through a remote member, the write and the denial
#[tokio::test(flavor = "multi_thread")]
async fn a_write_reaches_a_co_located_newcomer_the_writer_does_not_know_yet() -> Result<()> {
    let (mia_phone, tablet) = (node(QUIET, QUIET).await?, node(QUIET, QUIET).await?);
    let (mia, _) = host(&mia_phone).await?;
    let (bob, _) = host(&tablet).await?;
    let (dave, _) = host(&tablet).await?;
    let (erin, _) = host(&tablet).await?;
    let cell = found(&mia_phone, &mia).await?;
    invite(
        &mia_phone,
        &mia,
        cell,
        &bob,
        vec![device_of(&tablet, &bob)?],
    )
    .await?;
    let tickets = tickets(&mia_phone, &mia, cell).await?;
    tablet.import_cell(bob.id, cell, tickets.clone()).await?;
    assert!(lists(&tablet, bob.id, cell, mia.id, OWNER).await?);
    // Both out of both swarms and settled: Mia's node then dials nobody on
    // its own, so Dave's joined event reaches Bob's replica through no
    // session.
    for namespace in [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ] {
        tablet.leave_swarm_for_test(bob.id, namespace).await?;
        mia_phone.leave_swarm_for_test(mia.id, namespace).await?;
    }
    assert!(settle(&[&mia_phone, &tablet], cell).await?);
    invite(
        &mia_phone,
        &mia,
        cell,
        &dave,
        vec![device_of(&tablet, &dave)?],
    )
    .await?;
    tablet.import_cell(dave.id, cell, tickets.clone()).await?;
    tablet.import_cell(erin.id, cell, tickets).await?;
    assert!(lists(&tablet, dave.id, cell, bob.id, PLAIN).await?);
    assert_eq!(
        state_on(&tablet, bob.id, cell, dave.id).await,
        MemberState::default()
    );

    let claim = place_claim(&tablet, &bob, cell, 5).await?;
    assert!(
        reads(&tablet, dave.id, cell, claim).await?,
        "the write did not reach the co-located newcomer"
    );
    // Denied: the co-located ticket holder that is no member.
    assert!(holds_no_record(&tablet, erin.id, cell).await?);

    for node in [mia_phone, tablet] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A membership event reaching one of two co-located members, with nothing
/// written to the record store, opens both of the cell's sessions between
/// them on the next pass, and the other member reads a newcomer's record it
/// held and read nothing of.
///
/// Both members are out of every swarm and settled with the founder's node
/// before the newcomer's joined event is written, the founder's node is gone
/// once the newcomer has caught up, and the newcomer's node is dialed only
/// by the sessions named below, so what moves between the two is the pass's.
#[allow(clippy::too_many_lines)] // one scenario: the quiet pair, the membership event and both sessions
#[tokio::test(flavor = "multi_thread")]
async fn a_membership_event_with_no_record_written_opens_both_of_a_cells_sessions() -> Result<()> {
    let (mia_phone, erin_phone) = (node(QUIET, QUIET).await?, node(QUIET, QUIET).await?);
    let tablet = node(Duration::from_millis(300), QUIET).await?;
    let (mia, _) = host(&mia_phone).await?;
    let (erin, _) = host(&erin_phone).await?;
    let (bob, _) = host(&tablet).await?;
    let (dave, _) = host(&tablet).await?;
    let cell = found(&mia_phone, &mia).await?;
    for member in [&bob, &dave] {
        invite(
            &mia_phone,
            &mia,
            cell,
            member,
            vec![device_of(&tablet, member)?],
        )
        .await?;
    }
    let tickets = tickets(&mia_phone, &mia, cell).await?;
    let (membership, records) = (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    );
    for member in [&bob, &dave] {
        tablet.import_cell(member.id, cell, tickets.clone()).await?;
        assert!(lists(&tablet, member.id, cell, mia.id, OWNER).await?);
        for namespace in [membership, records] {
            tablet.leave_swarm_for_test(member.id, namespace).await?;
        }
    }
    assert!(settle(&[&mia_phone, &tablet], cell).await?);
    invite(
        &mia_phone,
        &mia,
        cell,
        &erin,
        vec![device_of(&erin_phone, &erin)?],
    )
    .await?;
    erin_phone.import_cell(erin.id, cell, tickets).await?;
    assert!(lists(&erin_phone, erin.id, cell, erin.id, PLAIN).await?);
    // Erin's phone serves both members once it knows their devices.
    for member in [&bob, &dave] {
        let device = device_of(&tablet, member)?;
        assert!(lists_device(&erin_phone, erin.id, cell, member.id, device).await?);
    }
    mia_phone.shutdown().await?;
    let claim = place_claim(&erin_phone, &erin, cell, 6).await?;
    // Dave's phone takes Erin's claim, and reads nothing of it.
    let named = |to: &SyncNode, addressed: &Person| {
        Contact::new(to.dial_handle().addr(), identity_of(addressed.id))
    };
    tablet
        .sync_namespace_as_for_test(
            dave.id,
            records,
            named(&erin_phone, &erin),
            identity_of(dave.id),
        )
        .await?;
    assert_eq!(tablet.read_cell_record(dave.id, cell, &claim).await?, None);

    // Quiet: the pass has reconciled the record store over what Dave took.
    let quiet = || tablet.co_located_pass_sessions_of(records);
    let mut settled = quiet();
    loop {
        let passes = tablet.reconcile_passes();
        assert!(eventually(|| async { Ok(tablet.reconcile_passes() >= passes + 3) }).await?);
        if quiet() == settled {
            break;
        }
        settled = quiet();
    }

    // Erin's joined event reaches Bob's replica alone, and no record moves.
    tablet
        .sync_namespace_as_for_test(
            bob.id,
            membership,
            named(&erin_phone, &erin),
            identity_of(bob.id),
        )
        .await?;
    assert!(lists(&tablet, bob.id, cell, erin.id, PLAIN).await?);
    let passes = tablet.reconcile_passes();
    assert!(
        eventually(|| async { Ok(quiet() > settled) }).await?,
        "a membership event opened no session of the record store: passes {passes} -> {}, membership sessions {}",
        tablet.reconcile_passes(),
        tablet.co_located_pass_sessions_of(membership)
    );
    assert!(reads(&tablet, dave.id, cell, claim).await?);

    for node in [erin_phone, tablet] {
        node.shutdown().await?;
    }
    Ok(())
}
