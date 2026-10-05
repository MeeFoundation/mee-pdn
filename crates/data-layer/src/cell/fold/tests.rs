use super::*;
use crate::cell::{
    keys::Seq,
    testing::{device, event, Cast, Person, Store},
};

const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};
const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};
const OUT: MemberState = MemberState {
    member: false,
    owner: false,
};

fn state(membership: &Membership, person: &Person) -> MemberState {
    membership
        .member(&person.id())
        .map(|member| member.state)
        .unwrap_or_default()
}

fn verdict(membership: &Membership, entry: usize) -> Verdict {
    membership.verdicts()[entry]
}

fn nothing(why: ForNothing) -> Verdict {
    Verdict::CountedForNothing(why)
}

/// The creator's founding event derives the cell's id and makes the creator
/// its one member and owner.
#[test]
fn a_founded_cell_lists_its_creator_as_owner() {
    let cast = Cast::new();
    let store = Store::founded_by(&cast.alice);
    assert_eq!(store.cell.to_string(), "9cbcbe4da7cc35a44360d64e45621957");
    let membership = store.fold();
    assert_eq!(state(&membership, &cast.alice), OWNER);
    assert!(membership.verdicts().iter().all(|v| *v == Verdict::Counted));
}

/// Any member invites, and a newcomer joins as a plain member. Denied: an
/// invite naming the inviter's point before its own join.
#[test]
fn any_member_invites_and_a_newcomer_joins_as_a_plain_member() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.bob, 1, &cast.carol);
    let before_its_join = store.invite(&cast.bob, 0, &cast.dave);
    let membership = store.fold();
    assert_eq!(state(&membership, &cast.bob), PLAIN);
    assert_eq!(state(&membership, &cast.carol), PLAIN);
    assert_eq!(state(&membership, &cast.dave), OUT);
    assert_eq!(
        verdict(&membership, before_its_join),
        nothing(ForNothing::ActorLacksState)
    );
}

/// An owner's promotion makes a member an owner. Denied: a plain member's
/// promotion of itself.
#[test]
fn an_owner_promotes_and_a_plain_member_does_not() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.carol);
    store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    let own = store.act(EventKind::Promoted, &cast.carol, 2, &cast.carol, 1);
    let membership = store.fold();
    assert_eq!(state(&membership, &cast.bob), OWNER);
    assert_eq!(state(&membership, &cast.carol), PLAIN);
    assert_eq!(
        verdict(&membership, own),
        nothing(ForNothing::ActorLacksState)
    );
}

/// What an owner did while an owner stands after its demotion. Denied: its
/// act naming its point after the demotion.
#[test]
fn what_an_owner_did_while_an_owner_stands_after_its_demotion() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    for newcomer in [&cast.bob, &cast.carol, &cast.dave] {
        store.invite(&cast.alice, 1, newcomer);
    }
    store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    store.act(EventKind::Promoted, &cast.carol, 2, &cast.bob, 2);
    store.act(EventKind::Demoted, &cast.bob, 3, &cast.alice, 1);
    let after_demotion = store.act(EventKind::Promoted, &cast.dave, 2, &cast.bob, 3);
    let membership = store.fold();
    assert_eq!(state(&membership, &cast.carol), OWNER);
    assert_eq!(state(&membership, &cast.bob), PLAIN);
    assert_eq!(state(&membership, &cast.dave), PLAIN);
    assert_eq!(
        verdict(&membership, after_demotion),
        nothing(ForNothing::ActorLacksState)
    );
}

/// A member's own leave takes it out. Denied: a leave another member writes
/// in its chain.
#[test]
fn a_member_leaves_for_itself_alone() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.carol);
    let for_another = store.act(EventKind::Left, &cast.bob, 2, &cast.carol, 1);
    let membership = store.fold();
    assert_eq!(state(&membership, &cast.bob), PLAIN);
    assert_eq!(
        verdict(&membership, for_another),
        nothing(ForNothing::WrongActor)
    );

    store.act(EventKind::Left, &cast.bob, 2, &cast.bob, 1);
    assert_eq!(state(&store.fold(), &cast.bob), OUT);
}

/// An owner's kick of a member and demotion of another owner apply.
/// Denied: the same acts on itself, and an invite of itself, count for
/// nothing, and the owner stays one.
#[test]
fn nobody_kicks_demotes_or_invites_itself() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    store.invite(alice, 1, bob);
    store.invite(alice, 1, carol);
    store.act(EventKind::Promoted, carol, 2, alice, 1);
    let kicked = store.act(EventKind::Kicked, bob, 2, alice, 1);
    let demoted = store.act(EventKind::Demoted, carol, 3, alice, 1);
    let kick = store.act(EventKind::Kicked, alice, 2, alice, 1);
    let demotion = store.act(EventKind::Demoted, alice, 2, alice, 1);
    let invite = store.join(alice, 1, alice, 2);
    let membership = store.fold();
    for entry in [kicked, demoted] {
        assert_eq!(verdict(&membership, entry), Verdict::Counted);
    }
    assert_eq!(state(&membership, bob), OUT);
    assert_eq!(state(&membership, carol), PLAIN);
    for entry in [kick, demotion, invite] {
        assert_eq!(verdict(&membership, entry), nothing(ForNothing::WrongActor));
    }
    assert_eq!(state(&membership, alice), OWNER);
}

/// A former owner that left and is invited again joins as a plain member,
/// its chain reading owner, out and plain member in turn.
#[test]
fn a_former_owner_invited_again_joins_as_a_plain_member() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.carol);
    store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    store.act(EventKind::Left, &cast.bob, 3, &cast.bob, 2);
    store.join(&cast.carol, 1, &cast.bob, 4);
    let membership = store.fold();
    let bob = membership.member(&cast.bob.id()).unwrap();
    assert_eq!(bob.state, PLAIN);
    assert_eq!(bob.state_at(2), Some(OWNER));
    assert_eq!(bob.state_at(3), Some(OUT));
    assert_eq!(bob.state_at(4), Some(PLAIN));
    assert_eq!(bob.state_at(5), None);
}

/// Entries arriving in any order fold into the same membership and give
/// each entry the same verdict.
#[test]
fn entries_in_any_order_fold_the_same() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    for newcomer in [&cast.bob, &cast.carol, &cast.dave] {
        store.invite(&cast.alice, 1, newcomer);
    }
    store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    store.act(EventKind::Promoted, &cast.carol, 2, &cast.bob, 2);
    store.act(EventKind::Demoted, &cast.bob, 3, &cast.alice, 1);
    store.act(EventKind::Kicked, &cast.dave, 2, &cast.carol, 2);
    store.act(EventKind::Promoted, &cast.dave, 3, &cast.bob, 3);
    store.statement(
        &cast.bob,
        2,
        &[cast.bob.device, device(0xb2)],
        device(0xb2).author,
    );
    let forward = store.fold();

    let count = store.entries.len();
    let orders: [Vec<usize>; 3] = [
        (0..count).rev().collect(),
        (0..count).map(|i| (i + 5) % count).collect(),
        (0..count)
            .filter(|i| i % 2 == 1)
            .chain((0..count).filter(|i| i % 2 == 0))
            .collect(),
    ];
    for order in orders {
        let entries: Vec<HeldEntry> = order.iter().map(|&i| store.entries[i].clone()).collect();
        let shuffled = Membership::fold(&store.cell, &entries);
        for person in [&cast.alice, &cast.bob, &cast.carol, &cast.dave] {
            assert_eq!(state(&shuffled, person), state(&forward, person));
        }
        for (position, &original) in order.iter().enumerate() {
            assert_eq!(shuffled.verdicts()[position], forward.verdicts()[original]);
        }
    }
}

/// A device statement counts on its embedded signature whoever writes it.
/// Denied: a statement for the same member signed by another member's key.
#[test]
fn a_device_statement_counts_whoever_writes_it() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    let relayed = store.statement(
        &cast.bob,
        2,
        &[cast.bob.device, device(0xb2)],
        cast.alice.author(),
    );
    let forged = store.write(
        MembershipKey::Devices {
            member: cast.bob.id(),
            version: 3,
        },
        cast.dave.author(),
        cast.dave
            .keys
            .device_statement(3, vec![cast.dave.device])
            .encode(),
    );
    let membership = store.fold();
    assert_eq!(verdict(&membership, relayed), Verdict::Counted);
    assert_eq!(
        verdict(&membership, forged),
        nothing(ForNothing::BadSignature)
    );
    let bob = membership.member(&cast.bob.id()).unwrap();
    assert_eq!(bob.devices, BTreeSet::from([cast.bob.device, device(0xb2)]));
}

/// Two statements at one version under two authors both count into the
/// device list. Denied: a third at that version under a wrong key, and a
/// statement moved to another version's key, add nothing.
#[test]
fn two_statements_at_one_version_both_count() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.carol);
    let (b2, b3) = (device(0xb2), device(0xb3));
    store.statement(&cast.bob, 2, &[cast.bob.device, b2], b2.author);
    store.statement(&cast.bob, 2, &[cast.bob.device, b3], b3.author);
    let wrong_key = store.write(
        MembershipKey::Devices {
            member: cast.bob.id(),
            version: 2,
        },
        cast.carol.author(),
        cast.carol
            .keys
            .device_statement(2, vec![cast.bob.device, device(0xb4)])
            .encode(),
    );
    let moved = store.write(
        MembershipKey::Devices {
            member: cast.bob.id(),
            version: 5,
        },
        cast.bob.author(),
        cast.bob
            .keys
            .device_statement(1, vec![device(0xb9)])
            .encode(),
    );
    let membership = store.fold();
    for entry in [wrong_key, moved] {
        assert_eq!(
            verdict(&membership, entry),
            nothing(ForNothing::BadSignature)
        );
    }
    let bob = membership.member(&cast.bob.id()).unwrap();
    assert_eq!(bob.devices, BTreeSet::from([cast.bob.device, b2, b3]));
}

/// A founding event counts only where it derives the cell id: another
/// member's, one in the creator's chain under another key, and a copy away
/// from the creator's first sequence count for nothing.
#[test]
fn founding_events_that_do_not_derive_the_cell_count_for_nothing() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    let bobs = store.write(
        MembershipKey::founded(cast.bob.id()),
        cast.bob.author(),
        cast.bob.keys.founding([0x5a; 16]).encode(),
    );
    let other_key = store.write(
        MembershipKey::founded(cast.alice.id()),
        cast.dave.author(),
        cast.dave.keys.founding([0x5a; 16]).encode(),
    );
    let moved = store.write(
        event(cast.alice.id(), 2, EventKind::Founded, cast.alice.id(), 0),
        cast.alice.author(),
        cast.alice.keys.founding([0x5a; 16]).encode(),
    );
    let membership = store.fold();
    assert_eq!(verdict(&membership, bobs), nothing(ForNothing::OtherCell));
    assert_eq!(
        verdict(&membership, other_key),
        nothing(ForNothing::OtherCell)
    );
    assert_eq!(verdict(&membership, moved), nothing(ForNothing::Misplaced));
    assert_eq!(state(&membership, &cast.alice), OWNER);
    assert_eq!(state(&membership, &cast.bob), PLAIN);
}

/// A device holding only a chain a modified device invented, rooted at
/// another creator, counts none of it.
#[test]
fn an_invented_founder_roots_nothing() {
    let cast = Cast::new();
    let mut invented = Store {
        cell: Store::founded_by(&cast.alice).cell,
        entries: Vec::new(),
    };
    let founding = invented.write(
        MembershipKey::founded(cast.bob.id()),
        cast.bob.author(),
        cast.bob.keys.founding([0x5a; 16]).encode(),
    );
    invented.statement(&cast.bob, 1, &[cast.bob.device], cast.bob.author());
    let joined = invented.invite(&cast.bob, 1, &cast.dave);
    let membership = invented.fold();
    assert_eq!(
        verdict(&membership, founding),
        nothing(ForNothing::OtherCell)
    );
    assert_eq!(
        verdict(&membership, joined),
        nothing(ForNothing::ActorLacksState)
    );
    assert_eq!(state(&membership, &cast.bob), OUT);
    assert_eq!(state(&membership, &cast.dave), OUT);
}

/// A joined event counts only under a key that derives its member's `PdnId`
/// and a statement over its own sequence: a departed member readmitted
/// under a key another device minted, a second join beside a member's
/// first, a join into the chain of an identity never a member, and a join
/// statement copied from an earlier join count for nothing, while the
/// member's own return counts.
#[test]
fn a_join_counts_only_under_its_members_own_statement() {
    let cast = Cast::new();
    let mallory = Person::new(0x66);
    let erin = Person::new(0xe0);
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.carol);
    store.act(EventKind::Left, &cast.carol, 2, &cast.carol, 1);
    let minted = |seq| {
        mallory
            .keys
            .join_statement(&store.cell, Seq::new(seq))
            .encode()
    };
    let (at_three, at_one) = (minted(3), minted(1));
    let copied = cast
        .carol
        .keys
        .join_statement(&store.cell, Seq::new(1))
        .encode();

    let readmitted = store.write(
        event(cast.carol.id(), 3, EventKind::Joined, cast.bob.id(), 1),
        cast.bob.author(),
        at_three,
    );
    let beside = store.write(
        event(cast.carol.id(), 1, EventKind::Joined, cast.bob.id(), 1),
        cast.bob.author(),
        at_one.clone(),
    );
    let never = store.write(
        event(erin.id(), 1, EventKind::Joined, cast.bob.id(), 1),
        cast.bob.author(),
        at_one,
    );
    let copy = store.write(
        event(cast.carol.id(), 3, EventKind::Joined, cast.alice.id(), 1),
        cast.alice.author(),
        copied,
    );
    let listing = store.write(
        MembershipKey::Devices {
            member: cast.carol.id(),
            version: 2,
        },
        cast.bob.author(),
        mallory
            .keys
            .device_statement(2, vec![cast.bob.device])
            .encode(),
    );
    let membership = store.fold();
    for entry in [readmitted, beside, never] {
        assert_eq!(
            verdict(&membership, entry),
            nothing(ForNothing::KeyOfAnother)
        );
    }
    assert_eq!(
        verdict(&membership, copy),
        nothing(ForNothing::BadSignature)
    );
    assert_eq!(
        verdict(&membership, listing),
        nothing(ForNothing::BadSignature)
    );
    assert_eq!(state(&membership, &cast.carol), OUT);
    assert_eq!(state(&membership, &erin), OUT);
    let carol = membership.member(&cast.carol.id()).unwrap();
    assert_eq!(carol.devices, BTreeSet::from([cast.carol.device]));

    let back = store.join(&cast.alice, 1, &cast.carol, 3);
    let membership = store.fold();
    assert_eq!(verdict(&membership, back), Verdict::Counted);
    assert_eq!(state(&membership, &cast.carol), PLAIN);
}

/// An event whose author no statement of its actor lists counts for
/// nothing, whoever relays it.
#[test]
fn an_event_by_no_device_of_its_actor_counts_for_nothing() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    let forged = store.write(
        event(cast.bob.id(), 2, EventKind::Promoted, cast.alice.id(), 1),
        cast.dave.author(),
        vec![0],
    );
    let membership = store.fold();
    assert_eq!(
        verdict(&membership, forged),
        nothing(ForNothing::AuthorNotActorDevice)
    );
    assert_eq!(state(&membership, &cast.bob), PLAIN);
}

/// An event naming an actor point the device does not hold waits, and
/// counts once the point arrives.
#[test]
fn an_event_ahead_of_its_actors_point_counts_once_the_point_arrives() {
    let cast = Cast::new();
    let mut early = Store::founded_by(&cast.alice);
    early.invite(&cast.alice, 1, &cast.bob);
    early.invite(&cast.alice, 1, &cast.carol);
    let ahead = early.act(EventKind::Promoted, &cast.carol, 2, &cast.bob, 2);
    let membership = early.fold();
    assert_eq!(
        verdict(&membership, ahead),
        Verdict::NotYet(Awaiting::ActorChain)
    );
    assert_eq!(state(&membership, &cast.carol), PLAIN);

    let mut full = early.clone();
    full.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    let membership = full.fold();
    assert_eq!(verdict(&membership, ahead), Verdict::Counted);
    assert_eq!(state(&membership, &cast.carol), OWNER);
}

/// An event whose payload has not arrived waits, and so does a device
/// statement ahead of the event carrying its member's key; both count once
/// the payload arrives.
#[test]
fn an_entry_waits_for_its_payload_and_a_statement_for_its_key() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    let joined = store.invite(&cast.alice, 1, &cast.bob);
    let statement = joined + 1;
    let mut early = store.clone();
    early.entries[joined].payload = None;
    let membership = early.fold();
    assert_eq!(
        verdict(&membership, joined),
        Verdict::NotYet(Awaiting::Payload)
    );
    assert_eq!(
        verdict(&membership, statement),
        Verdict::NotYet(Awaiting::AnnouncementKey)
    );
    assert_eq!(state(&membership, &cast.bob), OUT);

    let membership = store.fold();
    assert_eq!(verdict(&membership, joined), Verdict::Counted);
    assert_eq!(verdict(&membership, statement), Verdict::Counted);
    assert_eq!(state(&membership, &cast.bob), PLAIN);
}

/// Alice and Carol owners, Bob a plain member.
fn two_owners(cast: &Cast) -> Store {
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.carol);
    store.invite(&cast.alice, 1, &cast.bob);
    store.act(EventKind::Promoted, &cast.carol, 2, &cast.alice, 1);
    store
}

/// Events two owners write at one sequence of one member resolve by
/// precedence, the narrowest first: a kick outranks a promotion, a leave a
/// demotion, and one promotion written twice counts once.
#[test]
fn events_at_one_sequence_resolve_by_precedence() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);

    let mut store = two_owners(&cast);
    let promotion = store.act(EventKind::Promoted, bob, 2, alice, 1);
    let kick = store.act(EventKind::Kicked, bob, 2, carol, 2);
    let membership = store.fold();
    assert_eq!(verdict(&membership, promotion), Verdict::Counted);
    assert_eq!(verdict(&membership, kick), Verdict::Counted);
    assert_eq!(state(&membership, bob), OUT);

    let mut store = two_owners(&cast);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    store.act(EventKind::Demoted, bob, 3, carol, 2);
    store.act(EventKind::Left, bob, 3, bob, 2);
    assert_eq!(state(&store.fold(), bob), OUT);

    let mut store = two_owners(&cast);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    store.act(EventKind::Promoted, bob, 2, carol, 2);
    assert_eq!(state(&store.fold(), bob), OWNER);
}

/// A kick placed at the sequence where a member joined, and one placed at
/// the creator's first sequence, count for nothing: every member stays, the
/// newcomer the member invited among them.
#[test]
fn an_event_placed_at_a_joining_point_changes_nothing() {
    let cast = Cast::new();
    let mut store = two_owners(&cast);
    store.invite(&cast.bob, 1, &cast.dave);
    let at_bob = store.act(EventKind::Kicked, &cast.bob, 1, &cast.carol, 2);
    let at_alice = store.act(EventKind::Kicked, &cast.alice, 1, &cast.carol, 2);
    let membership = store.fold();
    for entry in [at_bob, at_alice] {
        assert_eq!(
            verdict(&membership, entry),
            nothing(ForNothing::TransitionNotAllowed)
        );
    }
    assert_eq!(state(&membership, &cast.alice), OWNER);
    assert_eq!(state(&membership, &cast.bob), PLAIN);
    assert_eq!(state(&membership, &cast.carol), OWNER);
    assert_eq!(state(&membership, &cast.dave), PLAIN);
}

/// An entry far beyond a chain waits and blocks nothing: the owner's next
/// act, naming its point before the gap, counts.
#[test]
fn an_entry_far_beyond_a_chain_waits_and_blocks_nothing() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.invite(&cast.alice, 1, &cast.dave);
    let far = store.act(EventKind::Promoted, &cast.alice, 1_000_000, &cast.dave, 1);
    let promotion = store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
    let membership = store.fold();
    assert_eq!(
        verdict(&membership, far),
        Verdict::NotYet(Awaiting::SubjectChain)
    );
    assert_eq!(verdict(&membership, promotion), Verdict::Counted);
    assert_eq!(state(&membership, &cast.bob), OWNER);
    assert_eq!(state(&membership, &cast.alice), OWNER);
}

/// Two owners demoting each other at once leave the one whose demotion
/// comes first in their actors' `PdnId` order an owner: the other demotion,
/// which would remove the last owner, is set aside.
#[test]
fn two_owners_demoting_each_other_leave_one_owner() {
    let cast = Cast::new();
    let mut store = two_owners(&cast);
    let alice_demotes_carol = store.act(EventKind::Demoted, &cast.carol, 3, &cast.alice, 1);
    let carol_demotes_alice = store.act(EventKind::Demoted, &cast.alice, 2, &cast.carol, 2);
    let membership = store.fold();
    let (standing, set_aside, owner, demoted) = if cast.carol.id() < cast.alice.id() {
        (
            carol_demotes_alice,
            alice_demotes_carol,
            &cast.carol,
            &cast.alice,
        )
    } else {
        (
            alice_demotes_carol,
            carol_demotes_alice,
            &cast.alice,
            &cast.carol,
        )
    };
    assert_eq!(verdict(&membership, standing), Verdict::Counted);
    assert_eq!(
        verdict(&membership, set_aside),
        nothing(ForNothing::LastOwnerKept)
    );
    assert_eq!(state(&membership, owner), OWNER);
    assert_eq!(state(&membership, demoted), PLAIN);
}

/// A chain past sequence 9 folds in number order, whatever order its
/// entries arrive in: the demotion at 10 applies after the promotion at 9.
#[test]
fn a_chain_past_nine_folds_in_number_order() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.invite(&cast.alice, 1, &cast.bob);
    store.act(EventKind::Demoted, &cast.bob, 10, &cast.alice, 1);
    for seq in 2..=9 {
        let kind = if seq % 2 == 0 || seq == 9 {
            EventKind::Promoted
        } else {
            EventKind::Demoted
        };
        store.act(kind, &cast.bob, seq, &cast.alice, 1);
    }
    let membership = store.fold();
    let bob = membership.member(&cast.bob.id()).unwrap();
    assert_eq!(bob.state_at(9), Some(OWNER));
    assert_eq!(bob.state, PLAIN);
}

/// Events that wait on each other's outcome in a loop count for nothing,
/// and so does one that waits on its own; the members they name keep the
/// state their chains held before.
#[test]
fn events_waiting_on_each_other_in_a_loop_count_for_nothing() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = two_owners(&cast);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    let kick = store.act(EventKind::Kicked, bob, 3, carol, 3);
    let demotion = store.act(EventKind::Demoted, carol, 3, bob, 3);
    let membership = store.fold();
    for entry in [kick, demotion] {
        assert_eq!(verdict(&membership, entry), nothing(ForNothing::Cyclic));
    }
    assert_eq!(state(&membership, bob), OWNER);
    assert_eq!(state(&membership, carol), OWNER);

    let mut store = two_owners(&cast);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    let own = store.act(EventKind::Promoted, bob, 3, bob, 3);
    let membership = store.fold();
    assert_eq!(verdict(&membership, own), nothing(ForNothing::Cyclic));
    assert_eq!(state(&membership, bob), OWNER);
}

/// A record key written into the membership store is kept apart and
/// changes no member.
#[test]
fn an_entry_outside_the_layout_changes_no_member() {
    let cast = Cast::new();
    let mut store = Store::founded_by(&cast.alice);
    store.entries.push(HeldEntry {
        key: format!(
            "by/{}/claim/1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d/1",
            cast.alice.id()
        )
        .into_bytes(),
        author: cast.alice.author(),
        payload: Some(vec![1]),
    });
    let membership = store.fold();
    assert_eq!(membership.verdicts().last(), Some(&Verdict::OutsideLayout));
    assert_eq!(state(&membership, &cast.alice), OWNER);
}

/// A row of a verdict class's table: what the case is, and its check.
type Case = (&'static str, fn(&Cast, &str));

/// What the fold counts on honest devices, one case per row over the
/// entries one device holds. The session that brings a newcomer's device
/// the store from nothing is `a_member_device_is_served_and_a_ticket_holder_is_not`
/// in `tests/cell_sessions.rs`. Paired: `rightly_counted_for_nothing`.
#[allow(clippy::too_many_lines)] // one table: every case of the class
#[test]
fn rightly_counted() {
    let cases: [Case; 9] = [
        ("the founding event deriving the cell id", |cast, case| {
            let membership = Store::founded_by(&cast.alice).fold();
            assert_eq!(verdict(&membership, 0), Verdict::Counted, "{case}");
            assert_eq!(state(&membership, &cast.alice), OWNER, "{case}");
        }),
        ("a join by a plain member", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let joined = store.invite(&cast.bob, 1, &cast.carol);
            let membership = store.fold();
            assert_eq!(verdict(&membership, joined), Verdict::Counted, "{case}");
            assert_eq!(state(&membership, &cast.carol), PLAIN, "{case}");
        }),
        (
            "a promotion by an owner naming its own point",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                let promoted = store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
                let membership = store.fold();
                assert_eq!(verdict(&membership, promoted), Verdict::Counted, "{case}");
                assert_eq!(state(&membership, &cast.bob), OWNER, "{case}");
            },
        ),
        (
            "a promotion by an owner demoted later, on a device linked after the demotion",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.invite(&cast.alice, 1, &cast.carol);
                store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
                let promoted = store.act(EventKind::Promoted, &cast.carol, 2, &cast.bob, 2);
                store.act(EventKind::Demoted, &cast.bob, 3, &cast.alice, 1);
                let membership = store.fold();
                assert_eq!(verdict(&membership, promoted), Verdict::Counted, "{case}");
                assert_eq!(state(&membership, &cast.carol), OWNER, "{case}");
                assert_eq!(state(&membership, &cast.bob), PLAIN, "{case}");
            },
        ),
        ("a leave by the member itself", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let left = store.act(EventKind::Left, &cast.bob, 2, &cast.bob, 1);
            let membership = store.fold();
            assert_eq!(verdict(&membership, left), Verdict::Counted, "{case}");
            assert_eq!(state(&membership, &cast.bob), OUT, "{case}");
        }),
        (
            "a former owner invited again by a plain member, as a plain member",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.invite(&cast.alice, 1, &cast.carol);
                store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
                store.act(EventKind::Left, &cast.bob, 3, &cast.bob, 2);
                let back = store.join(&cast.carol, 1, &cast.bob, 4);
                let membership = store.fold();
                assert_eq!(verdict(&membership, back), Verdict::Counted, "{case}");
                assert_eq!(state(&membership, &cast.bob), PLAIN, "{case}");
            },
        ),
        (
            "a newcomer's device folding from nothing, each statement before its join",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.invite(&cast.bob, 1, &cast.carol);
                let b2 = device(0xb2);
                store.statement(&cast.bob, 2, &[cast.bob.device, b2], b2.author);
                let backwards: Vec<HeldEntry> = store.entries.iter().rev().cloned().collect();
                let membership = Membership::fold(&store.cell, &backwards);
                assert!(
                    membership.verdicts().iter().all(|v| *v == Verdict::Counted),
                    "{case}"
                );
                assert_eq!(state(&membership, &cast.carol), PLAIN, "{case}");
                let bob = membership.member(&cast.bob.id()).unwrap();
                assert_eq!(bob.devices, BTreeSet::from([cast.bob.device, b2]), "{case}");
            },
        ),
        ("a device statement whoever relays it", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let b2 = device(0xb2);
            let relayed =
                store.statement(&cast.bob, 2, &[cast.bob.device, b2], cast.alice.author());
            let membership = store.fold();
            assert_eq!(verdict(&membership, relayed), Verdict::Counted, "{case}");
            let bob = membership.member(&cast.bob.id()).unwrap();
            assert!(bob.devices.contains(&b2), "{case}");
        }),
        (
            "two owners' events at one point, both held and resolved alike on every device",
            |cast, case| {
                let mut store = two_owners(cast);
                let promotion = store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
                let kick = store.act(EventKind::Kicked, &cast.bob, 2, &cast.carol, 2);
                let forward = store.fold();
                let backwards: Vec<HeldEntry> = store.entries.iter().rev().cloned().collect();
                let other_device = Membership::fold(&store.cell, &backwards);
                for membership in [&forward, &other_device] {
                    assert_eq!(state(membership, &cast.bob), OUT, "{case}");
                }
                for entry in [promotion, kick] {
                    assert_eq!(verdict(&forward, entry), Verdict::Counted, "{case}");
                    let mirrored = store.entries.len() - 1 - entry;
                    assert_eq!(verdict(&other_device, mirrored), Verdict::Counted, "{case}");
                }
            },
        ),
    ];
    let cast = Cast::new();
    for (case, check) in cases {
        check(&cast, case);
    }
}

/// What the fold holds and counts for nothing on honest devices, one case
/// per row over the entries one device holds. A kicked member's device
/// asking for a session is refused in
/// `a_member_device_is_served_and_a_ticket_holder_is_not` in
/// `tests/cell_sessions.rs`. Paired: `rightly_counted`.
#[allow(clippy::too_many_lines)] // one table: every case of the class
#[test]
fn rightly_counted_for_nothing() {
    let cases: [Case; 8] = [
        ("a promotion or a kick by a plain member", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            store.invite(&cast.alice, 1, &cast.carol);
            let promoted = store.act(EventKind::Promoted, &cast.bob, 2, &cast.carol, 1);
            let kicked = store.act(EventKind::Kicked, &cast.bob, 2, &cast.carol, 1);
            let membership = store.fold();
            for entry in [promoted, kicked] {
                assert_eq!(
                    verdict(&membership, entry),
                    nothing(ForNothing::ActorLacksState),
                    "{case}"
                );
            }
            assert_eq!(state(&membership, &cast.bob), PLAIN, "{case}");
        }),
        ("a leave by anyone but its subject", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let left = store.act(EventKind::Left, &cast.bob, 2, &cast.alice, 1);
            let membership = store.fold();
            assert_eq!(
                verdict(&membership, left),
                nothing(ForNothing::WrongActor),
                "{case}"
            );
            assert_eq!(state(&membership, &cast.bob), PLAIN, "{case}");
        }),
        ("a join, a kick or a demotion by its subject itself", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            let joined = store.join(&cast.alice, 1, &cast.alice, 2);
            let kicked = store.act(EventKind::Kicked, &cast.alice, 2, &cast.alice, 1);
            let demoted = store.act(EventKind::Demoted, &cast.alice, 2, &cast.alice, 1);
            let membership = store.fold();
            for entry in [joined, kicked, demoted] {
                assert_eq!(
                    verdict(&membership, entry),
                    nothing(ForNothing::WrongActor),
                    "{case}"
                );
            }
            assert_eq!(state(&membership, &cast.alice), OWNER, "{case}");
        }),
        (
            "a founding event that does not derive the cell id, in any order, served first to a device holding nothing",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.write(
                    MembershipKey::founded(cast.bob.id()),
                    cast.bob.author(),
                    cast.bob.keys.founding([0x5a; 16]).encode(),
                );
                let backwards: Vec<HeldEntry> = store.entries.iter().rev().cloned().collect();
                let membership = Membership::fold(&store.cell, &backwards);
                assert_eq!(
                    membership.verdicts()[0],
                    nothing(ForNothing::OtherCell),
                    "{case}"
                );
                assert_eq!(state(&membership, &cast.alice), OWNER, "{case}");

                let mut invented = Store {
                    cell: store.cell,
                    entries: Vec::new(),
                };
                let founding = invented.write(
                    MembershipKey::founded(cast.bob.id()),
                    cast.bob.author(),
                    cast.bob.keys.founding([0x5a; 16]).encode(),
                );
                let membership = invented.fold();
                assert_eq!(
                    verdict(&membership, founding),
                    nothing(ForNothing::OtherCell),
                    "{case}"
                );
                assert_eq!(state(&membership, &cast.bob), OUT, "{case}");
            },
        ),
        ("an event by a key no member's statement lists", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let forged = store.write(
                event(cast.bob.id(), 2, EventKind::Promoted, cast.alice.id(), 1),
                cast.dave.author(),
                vec![0],
            );
            let membership = store.fold();
            assert_eq!(
                verdict(&membership, forged),
                nothing(ForNothing::AuthorNotActorDevice),
                "{case}"
            );
            assert_eq!(state(&membership, &cast.bob), PLAIN, "{case}");
        }),
        ("a device statement under a wrong announcement key", |cast, case| {
            let mut store = Store::founded_by(&cast.alice);
            store.invite(&cast.alice, 1, &cast.bob);
            let forged = store.write(
                MembershipKey::Devices {
                    member: cast.bob.id(),
                    version: 2,
                },
                cast.dave.author(),
                cast.dave
                    .keys
                    .device_statement(2, vec![cast.dave.device])
                    .encode(),
            );
            let membership = store.fold();
            assert_eq!(
                verdict(&membership, forged),
                nothing(ForNothing::BadSignature),
                "{case}"
            );
            let bob = membership.member(&cast.bob.id()).unwrap();
            assert_eq!(bob.devices, BTreeSet::from([cast.bob.device]), "{case}");
        }),
        (
            "a join under a key that does not derive its member's PdnId, or with a copied join statement",
            |cast, case| {
                let mallory = Person::new(0x66);
                let erin = Person::new(0xe0);
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.invite(&cast.alice, 1, &cast.carol);
                store.act(EventKind::Left, &cast.carol, 2, &cast.carol, 1);
                let minted = |seq| {
                    mallory
                        .keys
                        .join_statement(&store.cell, Seq::new(seq))
                        .encode()
                };
                let (at_three, at_one) = (minted(3), minted(1));
                let copied = cast
                    .carol
                    .keys
                    .join_statement(&store.cell, Seq::new(1))
                    .encode();
                let next_sequence = store.write(
                    event(cast.carol.id(), 3, EventKind::Joined, cast.bob.id(), 1),
                    cast.bob.author(),
                    at_three,
                );
                let first_sequence = store.write(
                    event(cast.carol.id(), 1, EventKind::Joined, cast.bob.id(), 1),
                    cast.bob.author(),
                    at_one.clone(),
                );
                let never_member = store.write(
                    event(erin.id(), 1, EventKind::Joined, cast.bob.id(), 1),
                    cast.bob.author(),
                    at_one,
                );
                let copy = store.write(
                    event(cast.carol.id(), 3, EventKind::Joined, cast.alice.id(), 1),
                    cast.alice.author(),
                    copied,
                );
                let membership = store.fold();
                for entry in [next_sequence, first_sequence, never_member] {
                    assert_eq!(
                        verdict(&membership, entry),
                        nothing(ForNothing::KeyOfAnother),
                        "{case}"
                    );
                }
                assert_eq!(
                    verdict(&membership, copy),
                    nothing(ForNothing::BadSignature),
                    "{case}"
                );
                assert_eq!(state(&membership, &cast.carol), OUT, "{case}");
                assert_eq!(state(&membership, &erin), OUT, "{case}");
            },
        ),
        (
            "an event naming an actor point the device does not hold, until the point arrives",
            |cast, case| {
                let mut store = Store::founded_by(&cast.alice);
                store.invite(&cast.alice, 1, &cast.bob);
                store.invite(&cast.alice, 1, &cast.carol);
                let ahead = store.act(EventKind::Promoted, &cast.carol, 2, &cast.bob, 2);
                let membership = store.fold();
                assert_eq!(
                    verdict(&membership, ahead),
                    Verdict::NotYet(Awaiting::ActorChain),
                    "{case}"
                );
                assert_eq!(state(&membership, &cast.carol), PLAIN, "{case}");

                store.act(EventKind::Promoted, &cast.bob, 2, &cast.alice, 1);
                let membership = store.fold();
                assert_eq!(verdict(&membership, ahead), Verdict::Counted, "{case}");
                assert_eq!(state(&membership, &cast.carol), OWNER, "{case}");
            },
        ),
    ];
    let cast = Cast::new();
    for (case, check) in cases {
        check(&cast, case);
    }
}

/// An event whose actor point no live device holds — the owner that wrote
/// that point died before spreading it — is never counted, and its subject
/// becomes an owner only through a promotion a current owner issues anew at
/// a point every device holds. Paired: that promotion counts.
#[test]
fn wrongly_left_uncounted_without_anchoring_d23() {
    let cast = Cast::new();
    let (alice, bob, carol, dave) = (&cast.alice, &cast.bob, &cast.carol, &cast.dave);
    let mut store = Store::founded_by(alice);
    for newcomer in [bob, carol, dave] {
        store.invite(alice, 1, newcomer);
    }
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    // Bob's promotion of Carol at Carol's sequence 2 reached Carol's device
    // alone, and both devices died: no device here holds that point.
    let stranded = store.act(EventKind::Promoted, dave, 2, carol, 2);
    let membership = store.fold();
    assert_eq!(
        verdict(&membership, stranded),
        Verdict::NotYet(Awaiting::ActorChain)
    );
    assert_eq!(state(&membership, dave), PLAIN);

    let anew = store.act(EventKind::Promoted, dave, 3, alice, 1);
    let membership = store.fold();
    assert_eq!(verdict(&membership, anew), Verdict::Counted);
    assert_eq!(
        verdict(&membership, stranded),
        Verdict::NotYet(Awaiting::ActorChain)
    );
    assert_eq!(state(&membership, dave), OWNER);
}
