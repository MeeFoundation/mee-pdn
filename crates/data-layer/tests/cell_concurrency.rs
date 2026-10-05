//! Acts written at one point of a chain by member devices out of reach of
//! each other, and events reaching a device out of their order: once the
//! devices meet, every one holds every entry and folds the same membership,
//! by precedence and by the guard over demotions. The acts arrive by the
//! store-level writes the cells service performs, the tickets by hand.

use std::time::Duration;

use anyhow::{Context as _, Result};
use data_layer::{
    identity_of, Awaiting, CellStore, CellTickets, CellVerdicts, Contact, EventKind, MemberState,
    MembershipKey, OpId, RecordKey, Seq, SpawnOptions, SyncNode, Verdict, ACT_PAYLOAD,
};
use pdn_types::{CellId, RecordId};
use test_utils::{
    cell::{device_of, found, host, invite, lists, reads, tickets, write, Person},
    TIMEOUT,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};
const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};
const OUT: MemberState = MemberState {
    member: false,
    owner: false,
};

async fn node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        cell_reconcile_interval: QUIET,
        ..SpawnOptions::memory()
    })
    .await
}

/// A dial of `cell`'s membership store from `holder`'s replica on `from`
/// to `callee`'s on `to`, as a drawn contact is dialed.
async fn dial(
    from: &SyncNode,
    holder: &Person,
    cell: CellId,
    to: &SyncNode,
    callee: &Person,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee.id));
    from.sync_cell_with_for_test(holder.id, cell, CellStore::Membership, contact)
        .await
}

/// `actor`'s act of `kind` in `subject`'s chain at `seq`, naming the
/// actor's `actor_seq`, written from the actor's device on `node`.
async fn act(
    node: &SyncNode,
    actor: &Person,
    actor_seq: u64,
    kind: EventKind,
    subject: &Person,
    seq: u64,
    cell: CellId,
) -> Result<Vec<u8>> {
    let key = MembershipKey::Event {
        subject: subject.id,
        seq: Seq::new(seq),
        kind,
        actor: actor.id,
        actor_seq: Seq::new(actor_seq),
    };
    let bytes = key.to_bytes();
    write(node, actor, cell, key, ACT_PAYLOAD.to_vec()).await?;
    Ok(bytes)
}

/// A cell the person on the first of `phones` founds, a person on each
/// other phone invited by the founder, at the founder's sequence 1, and
/// every one of them holding both stores.
async fn cell_on(phones: &[&SyncNode]) -> Result<(Vec<Person>, CellId, CellTickets)> {
    let mut people = Vec::new();
    for phone in phones {
        people.push(host(phone).await?.0);
    }
    let (Some(founder_phone), Some(founder)) = (phones.first(), people.first()) else {
        anyhow::bail!("a cell needs its founder");
    };
    let cell = found(founder_phone, founder).await?;
    for (phone, member) in phones.iter().zip(&people).skip(1) {
        let devices = vec![device_of(phone, member)?];
        invite(founder_phone, founder, cell, member, devices).await?;
    }
    let tickets = tickets(founder_phone, founder, cell).await?;
    for (phone, member) in phones.iter().zip(&people).skip(1) {
        phone.import_cell(member.id, cell, tickets.clone()).await?;
        for other in people.iter().skip(1) {
            assert!(lists(phone, member.id, cell, other.id, PLAIN).await?);
        }
    }
    Ok((people, cell, tickets))
}

/// Every phone out of both of `cell`'s swarms, with nothing in flight, so
/// what it writes next stays on it until a scenario dials.
async fn out_of_reach(
    phones: &[&SyncNode],
    people: &[Person],
    cell: CellId,
    tickets: &CellTickets,
) -> Result<()> {
    for (phone, member) in phones.iter().zip(people) {
        for namespace in [
            tickets.membership.capability.id(),
            tickets.records.capability.id(),
        ] {
            phone.leave_swarm_for_test(member.id, namespace).await?;
        }
    }
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut quiet_reads = 0_u8;
    while quiet_reads < 2 {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the cell's stores never settled"
        );
        let mut in_flight = 0_usize;
        for phone in phones {
            in_flight = in_flight.saturating_add(phone.cell_syncs_in_flight_for_test(cell).await?);
        }
        quiet_reads = if in_flight == 0 {
            quiet_reads.saturating_add(1)
        } else {
            0
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Whether `holder`'s replica on `node` comes to hold every key of `keys`
/// in its membership store, each verdict read from the fold a read
/// reports on `reports`.
async fn holds(
    node: &SyncNode,
    reports: &mut UnboundedReceiver<CellVerdicts>,
    holder: &Person,
    cell: CellId,
    keys: &[Vec<u8>],
) -> Result<bool> {
    verdicts_come_to(node, reports, holder, cell, |held| {
        keys.iter()
            .all(|key| held.iter().any(|(held, _verdict)| held == key))
    })
    .await
}

async fn verdicts_come_to(
    node: &SyncNode,
    reports: &mut UnboundedReceiver<CellVerdicts>,
    holder: &Person,
    cell: CellId,
    mut want: impl FnMut(&[(Vec<u8>, Verdict)]) -> bool,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        node.cell_membership(holder.id, cell).await?;
        let mut held = None;
        while let Ok(report) = reports.try_recv() {
            if report.identity == holder.id && report.cell == cell {
                held = Some(report.verdicts);
            }
        }
        let held: Vec<(Vec<u8>, Verdict)> = held
            .context("the read reported no fold")?
            .into_iter()
            .map(|(key, _author, verdict)| (key, verdict))
            .collect();
        if want(&held) {
            return Ok(true);
        }
        if tokio::time::Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A promotion and a kick two owners write at one point of a member's
/// chain while out of reach of each other take the member out on every
/// member device, and so do a demotion and the subject's own leave at one
/// point of its chain, every member device holding all four entries.
#[allow(clippy::too_many_lines)] // one scenario: two pairs of acts at one point and their meeting
#[tokio::test(flavor = "multi_thread")]
async fn acts_at_one_point_resolve_by_precedence_on_every_member_device() -> Result<()> {
    let phones = [node().await?, node().await?, node().await?, node().await?];
    let [alice_phone, bob_phone, carol_phone, dave_phone] = &phones;
    let mut reports: Vec<_> = phones
        .iter()
        .map(|phone| phone.take_cell_verdicts().context("taken once"))
        .collect::<Result<_>>()?;
    let all = [alice_phone, bob_phone, carol_phone, dave_phone];
    let (people, cell, tickets) = cell_on(&all).await?;
    let [alice, bob, carol, dave] = people.as_slice() else {
        anyhow::bail!("four members expected");
    };
    for owner in [carol, dave] {
        act(alice_phone, alice, 1, EventKind::Promoted, owner, 2, cell).await?;
    }
    for (phone, member) in [(carol_phone, carol), (dave_phone, dave)] {
        dial(phone, member, cell, alice_phone, alice).await?;
        assert!(lists(phone, member.id, cell, member.id, OWNER).await?);
    }
    out_of_reach(&all, &people, cell, &tickets).await?;

    let written = [
        act(alice_phone, alice, 1, EventKind::Promoted, bob, 2, cell).await?,
        act(carol_phone, carol, 2, EventKind::Kicked, bob, 2, cell).await?,
        act(alice_phone, alice, 1, EventKind::Demoted, dave, 3, cell).await?,
        act(dave_phone, dave, 2, EventKind::Left, dave, 3, cell).await?,
    ];
    // Alice's phone meets Carol's, then the departed Dave's, which serves it
    // the leave's past; Carol's phone takes the leave from Alice's.
    dial(alice_phone, alice, cell, carol_phone, carol).await?;
    dial(alice_phone, alice, cell, dave_phone, dave).await?;
    assert!(lists(alice_phone, alice.id, cell, dave.id, OUT).await?);
    dial(carol_phone, carol, cell, alice_phone, alice).await?;

    for (index, phone, holder) in [(0, alice_phone, alice), (2, carol_phone, carol)] {
        let reports = reports.get_mut(index).context("one channel per phone")?;
        assert!(
            holds(phone, reports, holder, cell, &written).await?,
            "a member device dropped one of two acts at one point"
        );
        for (member, want) in [(alice, OWNER), (carol, OWNER), (bob, OUT), (dave, OUT)] {
            assert!(lists(phone, holder.id, cell, member.id, want).await?);
        }
    }
    // Only once Alice's phone holds the kick: a dial returns when asked for,
    // and its session serves what Alice's phone held as it opened.
    dial(bob_phone, bob, cell, alice_phone, alice).await?;
    assert!(
        lists(bob_phone, bob.id, cell, bob.id, OUT).await?,
        "the kicked member's device did not take the kick that outranks its promotion"
    );

    for phone in phones {
        phone.shutdown().await?;
    }
    Ok(())
}

/// Two owners, the cell's only ones, demoting each other while out of reach
/// of each other leave on every member device the owner whose `PdnId` sorts
/// lower an owner, its demotion standing, and the other a plain member.
#[tokio::test(flavor = "multi_thread")]
async fn two_owners_demoting_each_other_leave_the_one_whose_demotion_stood() -> Result<()> {
    let phones = [node().await?, node().await?, node().await?];
    let [alice_phone, bob_phone, carol_phone] = &phones;
    let mut reports: Vec<_> = phones
        .iter()
        .map(|phone| phone.take_cell_verdicts().context("taken once"))
        .collect::<Result<_>>()?;
    let all = [alice_phone, bob_phone, carol_phone];
    let (people, cell, tickets) = cell_on(&all).await?;
    let [alice, bob, carol] = people.as_slice() else {
        anyhow::bail!("three members expected");
    };
    act(alice_phone, alice, 1, EventKind::Promoted, carol, 2, cell).await?;
    dial(carol_phone, carol, cell, alice_phone, alice).await?;
    assert!(lists(carol_phone, carol.id, cell, carol.id, OWNER).await?);
    out_of_reach(&all, &people, cell, &tickets).await?;

    let written = [
        act(alice_phone, alice, 1, EventKind::Demoted, carol, 3, cell).await?,
        act(carol_phone, carol, 2, EventKind::Demoted, alice, 2, cell).await?,
    ];
    dial(bob_phone, bob, cell, alice_phone, alice).await?;
    dial(bob_phone, bob, cell, carol_phone, carol).await?;
    dial(alice_phone, alice, cell, carol_phone, carol).await?;

    let (alice_stays, carol_stays) = if alice.id < carol.id {
        (OWNER, PLAIN)
    } else {
        (PLAIN, OWNER)
    };
    for (index, phone, holder) in [
        (0, alice_phone, alice),
        (1, bob_phone, bob),
        (2, carol_phone, carol),
    ] {
        let reports = reports.get_mut(index).context("one channel per phone")?;
        assert!(holds(phone, reports, holder, cell, &written).await?);
        assert!(lists(phone, holder.id, cell, alice.id, alice_stays).await?);
        assert!(lists(phone, holder.id, cell, carol.id, carol_stays).await?);
        assert!(lists(phone, holder.id, cell, bob.id, PLAIN).await?);
    }

    for phone in phones {
        phone.shutdown().await?;
    }
    Ok(())
}

/// The last two owners leaving while out of reach of each other leave the
/// cell without an owner: the plain member that remains reads what was
/// placed and appends to a mergeable-document. Denied: its own promotion
/// of itself, held and counted for nothing.
#[allow(clippy::too_many_lines)] // one scenario: the two leaves, the meeting and the cell after it
#[tokio::test(flavor = "multi_thread")]
async fn the_last_two_owners_leaving_at_once_leave_the_cell_without_an_owner() -> Result<()> {
    let phones = [node().await?, node().await?, node().await?];
    let [alice_phone, bob_phone, carol_phone] = &phones;
    let mut bobs = bob_phone.take_cell_verdicts().context("taken once")?;
    let all = [alice_phone, bob_phone, carol_phone];
    let (people, cell, tickets) = cell_on(&all).await?;
    let [alice, bob, carol] = people.as_slice() else {
        anyhow::bail!("three members expected");
    };
    act(alice_phone, alice, 1, EventKind::Promoted, carol, 2, cell).await?;
    dial(carol_phone, carol, cell, alice_phone, alice).await?;
    assert!(lists(carol_phone, carol.id, cell, carol.id, OWNER).await?);
    let claim = RecordKey::Claim {
        member: alice.id,
        id: RecordId::from_bytes([1; 16]),
        mseq: Seq::FIRST,
    };
    let note = |writer: &Person, phone: &SyncNode, mseq: u64| -> Result<RecordKey> {
        Ok(RecordKey::Operation {
            member: alice.id,
            id: RecordId::from_bytes([2; 16]),
            op: OpId {
                writer: writer.id,
                author: device_of(phone, writer)?.author,
                mseq: Seq::new(mseq),
                op_seq: 1,
            },
        })
    };
    for (key, payload) in [
        (claim, &b"claim"[..]),
        (note(alice, alice_phone, 1)?, b"milk"),
    ] {
        alice_phone
            .write_cell_entry(alice.id, cell, CellStore::Records, &key.to_bytes(), payload)
            .await?;
    }
    dial(bob_phone, bob, cell, alice_phone, alice).await?;
    let contact = Contact::new(alice_phone.dial_handle().addr(), identity_of(alice.id));
    bob_phone
        .sync_cell_with_for_test(bob.id, cell, CellStore::Records, contact)
        .await?;
    assert!(reads(bob_phone, bob.id, cell, claim.record()).await?);
    out_of_reach(&all, &people, cell, &tickets).await?;

    act(alice_phone, alice, 1, EventKind::Left, alice, 2, cell).await?;
    act(carol_phone, carol, 2, EventKind::Left, carol, 3, cell).await?;
    dial(bob_phone, bob, cell, alice_phone, alice).await?;
    dial(bob_phone, bob, cell, carol_phone, carol).await?;
    for (member, want) in [(alice, OUT), (carol, OUT), (bob, PLAIN)] {
        assert!(lists(bob_phone, bob.id, cell, member.id, want).await?);
    }
    let owners = bob_phone
        .cell_membership(bob.id, cell)
        .await?
        .identities()
        .filter(|(_id, member)| member.state.owner)
        .count();
    assert_eq!(owners, 0, "the cell kept an owner after both owners left");

    assert!(reads(bob_phone, bob.id, cell, claim.record()).await?);
    let edit = note(bob, bob_phone, 1)?;
    bob_phone
        .write_cell_entry(bob.id, cell, CellStore::Records, &edit.to_bytes(), b"eggs")
        .await?;
    let read: Vec<_> = bob_phone
        .read_cell_operations(bob.id, cell, &edit.record())
        .await?
        .into_iter()
        .map(|operation| (operation.id.writer, operation.payload))
        .collect();
    assert_eq!(
        read,
        {
            let mut both = vec![(alice.id, b"milk".to_vec()), (bob.id, b"eggs".to_vec())];
            both.sort();
            both
        },
        "the plain member left without an owner does not edit"
    );
    // Denied: the plain member's promotion of itself.
    let promoted = act(bob_phone, bob, 1, EventKind::Promoted, bob, 2, cell).await?;
    assert!(
        verdicts_come_to(bob_phone, &mut bobs, bob, cell, |held| {
            held.iter().any(|(key, verdict)| {
                *key == promoted && matches!(verdict, Verdict::CountedForNothing(_))
            })
        })
        .await?
    );
    assert!(lists(bob_phone, bob.id, cell, bob.id, PLAIN).await?);

    for phone in phones {
        phone.shutdown().await?;
    }
    Ok(())
}

/// A member's promotion, demotion and second promotion reaching a device in
/// the order 4, 2, 3 resolve by sequence: the member stays a plain member
/// while the promotion at 4 waits for the sequences below it, is an owner
/// once 2 arrives, and an owner once all three have. The writing device
/// writes the events in the order the other device takes them, one dial
/// each.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_flip_resolves_by_sequence_whatever_the_arrival_order() -> Result<()> {
    let phones = [node().await?, node().await?, node().await?];
    let [alice_phone, bob_phone, carol_phone] = &phones;
    let mut carols = carol_phone.take_cell_verdicts().context("taken once")?;
    let all = [alice_phone, bob_phone, carol_phone];
    let (people, cell, tickets) = cell_on(&all).await?;
    let [alice, bob, carol] = people.as_slice() else {
        anyhow::bail!("three members expected");
    };
    out_of_reach(&all, &people, cell, &tickets).await?;

    let last = act(alice_phone, alice, 1, EventKind::Promoted, bob, 4, cell).await?;
    dial(carol_phone, carol, cell, alice_phone, alice).await?;
    assert!(
        verdicts_come_to(carol_phone, &mut carols, carol, cell, |held| {
            held.iter().any(|(key, verdict)| {
                *key == last && *verdict == Verdict::NotYet(Awaiting::SubjectChain)
            })
        })
        .await?,
        "the promotion at 4 did not wait for the sequences below it"
    );
    assert!(lists(carol_phone, carol.id, cell, bob.id, PLAIN).await?);

    act(alice_phone, alice, 1, EventKind::Promoted, bob, 2, cell).await?;
    dial(carol_phone, carol, cell, alice_phone, alice).await?;
    assert!(lists(carol_phone, carol.id, cell, bob.id, OWNER).await?);

    let demoted = act(alice_phone, alice, 1, EventKind::Demoted, bob, 3, cell).await?;
    dial(carol_phone, carol, cell, alice_phone, alice).await?;
    assert!(holds(carol_phone, &mut carols, carol, cell, &[demoted, last]).await?);
    assert!(lists(carol_phone, carol.id, cell, bob.id, OWNER).await?);
    let run = carol_phone
        .cell_membership(carol.id, cell)
        .await?
        .member(&bob.id)
        .map(data_layer::Member::run);
    assert_eq!(run, Some(4), "the chain did not fold through all four");

    for phone in phones {
        phone.shutdown().await?;
    }
    Ok(())
}
