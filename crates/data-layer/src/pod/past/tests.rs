use super::*;
use crate::pod::{
    testing::{event, Cast, Person, Store},
    EventKind,
};

fn past_of(store: &Store, member: &Person) -> Option<HashSet<PastEntry>> {
    departure_past(&store.membership_view(), &store.entries, &member.id())
}

fn entry(store: &Store, index: usize) -> PastEntry {
    let held = &store.entries[index];
    (held.key.clone(), held.author)
}

/// A removal's past is the removed event, the removed member's chain before it
/// and the owner's chain up to the point it names, with what resolves each
/// author, down to the created event. Denied: another member's join and a
/// newcomer the removal does not rest on are left out.
#[test]
fn a_removals_past_holds_what_it_rests_on_and_nothing_else() {
    let cast = Cast::new();
    let (alice, bob, carol, dave) = (&cast.alice, &cast.bob, &cast.carol, &cast.dave);
    let mut store = Store::created_by(alice);
    let bob_joined = store.invite(alice, 1, bob);
    let carol_joined = store.invite(alice, 1, carol);
    let removal = store.act(EventKind::Removed, carol, 2, alice, 1);
    let dave_joined = store.invite(bob, 1, dave);
    let past = past_of(&store, carol).expect("Carol departed");
    for (index, name) in [
        (0, "the created event"),
        (1, "the creator's statement"),
        (carol_joined, "Carol's joined event"),
        (removal, "the removal"),
    ] {
        assert!(past.contains(&entry(&store, index)), "{name} left out");
    }
    for (index, name) in [
        (bob_joined, "Bob's joined event"),
        (dave_joined, "Dave's joined event"),
    ] {
        assert!(!past.contains(&entry(&store, index)), "{name} taken in");
    }
    assert_eq!(past.len(), 4);
}

/// A leave's past holds the member's own chain, its join naming the device
/// it left from, and what its join rests on. Denied: nobody else's entries.
#[test]
fn a_leaves_past_holds_the_members_own_chain_and_what_it_rests_on() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::created_by(alice);
    let bob_joined = store.invite(alice, 1, bob);
    let carol_joined = store.invite(alice, 1, carol);
    let left = store.act(EventKind::Left, carol, 2, carol, 1);
    let past = past_of(&store, carol).expect("Carol departed");
    for index in [0, 1, carol_joined, left] {
        assert!(past.contains(&entry(&store, index)));
    }
    assert!(!past.contains(&entry(&store, bob_joined)));
    assert_eq!(past.len(), 4);
}

/// A sequence held by an entry that counts for nothing stays in the past,
/// so the former member's device walks the chain as far as the member
/// devices do.
#[test]
fn an_earlier_sequence_held_by_an_entry_counting_for_nothing_stays_in_the_past() {
    let cast = Cast::new();
    let (alice, carol) = (&cast.alice, &cast.carol);
    let mut store = Store::created_by(alice);
    store.invite(alice, 1, carol);
    let own_promotion = store.act(EventKind::Promoted, carol, 2, carol, 1);
    let removal = store.act(EventKind::Removed, carol, 3, alice, 1);
    let past = past_of(&store, carol).expect("Carol departed");
    assert!(past.contains(&entry(&store, own_promotion)));
    assert!(past.contains(&entry(&store, removal)));
}

/// A member and an identity that never joined have no departure's past.
#[test]
fn no_past_without_a_departure() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::created_by(alice);
    store.invite(alice, 1, bob);
    store.write(
        event(carol.id(), 1, EventKind::Joined, bob.id(), 0),
        bob.author(),
        vec![0],
    );
    assert!(past_of(&store, bob).is_none());
    assert!(past_of(&store, carol).is_none());
}

/// A departure stays where it is whatever lands in the chain after it: an
/// owner's promotion and second removal written while that owner had not
/// seen the first, which outranks the promotion, and an event no member
/// could write.
#[test]
fn an_entry_after_a_departure_hides_it_from_nobody() {
    let cast = Cast::new();
    let (alice, bob, carol, dave) = (&cast.alice, &cast.bob, &cast.carol, &cast.dave);
    let mut store = Store::created_by(alice);
    store.invite(alice, 1, bob);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    store.invite(alice, 1, carol);
    let removal = store.act(EventKind::Removed, carol, 2, alice, 1);
    store.act(EventKind::Promoted, carol, 2, bob, 2);
    let second = store.act(EventKind::Removed, carol, 3, bob, 2);
    let past = past_of(&store, carol).expect("Carol departed");
    assert!(past.contains(&entry(&store, removal)));
    assert!(!past.contains(&entry(&store, second)));

    // A joined event under a key that derives another `PdnId`.
    let forged = dave
        .keys
        .join_statement(&store.pod, crate::pod::Seq::new(4), dave.device)
        .encode();
    store.write(
        event(carol.id(), 4, EventKind::Joined, bob.id(), 2),
        bob.author(),
        forged,
    );
    assert_eq!(
        store
            .membership_view()
            .member(&carol.id())
            .and_then(crate::pod::Member::departed_at),
        Some(2)
    );
    assert!(past_of(&store, carol).is_some_and(|past| past.contains(&entry(&store, removal))));
}

fn rests_of(store: &Store, member: &Person) -> Rests {
    Rests::of(&store.entries, &member.id())
}

fn key_of(store: &Store, index: usize) -> Vec<u8> {
    store.entries[index].key.clone()
}

/// A removed member's tombstone takes an event it does not hold at a point
/// of the remover's chain its removal names — the one that reverses the
/// removal — and, once it holds that event, the chain of its actor up to
/// the point it names. Denied: a newcomer's join and statement, and the
/// reversing actor's chain past its point.
#[test]
fn a_tombstone_takes_what_may_reverse_its_departure_one_layer_a_session() {
    let cast = Cast::new();
    let (alice, bob, carol, dave) = (&cast.alice, &cast.bob, &cast.carol, &cast.dave);
    let mut members = Store::created_by(alice);
    members.invite(alice, 1, bob);
    members.invite(alice, 1, carol);
    let carols_promotion = members.act(EventKind::Promoted, carol, 2, alice, 1);
    members.invite(alice, 1, dave);
    members.act(EventKind::Promoted, bob, 2, alice, 1);
    members.act(EventKind::Removed, dave, 2, bob, 2);
    let mut tombstone = members.clone();
    let reversal = members.act(EventKind::Removed, bob, 2, carol, 2);
    let carols_later = members.act(EventKind::Demoted, carol, 3, alice, 1);
    let newcomer = Person::new(0xe0);
    let newcomer_joined = members.invite(alice, 1, &newcomer);
    let newcomers = members.statement(&newcomer, 1, &[newcomer.device], newcomer.author());

    let rests = rests_of(&tombstone, dave);
    assert!(rests.admits(&key_of(&members, reversal)));
    assert!(!rests.admits(&key_of(&members, newcomer_joined)));
    assert!(!rests.admits(&key_of(&members, newcomers)));
    // Carol's chain is no point Dave's removal rests on until the reversal
    // is held.
    assert!(!rests.admits(&key_of(&members, carols_promotion)));
    assert!(members
        .membership_view()
        .member(&dave.id())
        .is_some_and(|member| member.state.member));

    tombstone.entries.push(members.entries[reversal].clone());
    let rests = rests_of(&tombstone, dave);
    assert!(rests.admits(&key_of(&members, carols_promotion)));
    assert!(!rests.admits(&key_of(&members, carols_later)));
    assert!(!rests.admits(&key_of(&members, newcomer_joined)));
}

/// A departed member's tombstone takes its own chain's later events — a new
/// join — and, once it holds the join, the inviter's chain up to the point
/// it names, however far that chain ran after the departure. Denied: the
/// inviter's chain past that point.
#[test]
fn a_tombstone_takes_a_new_join_and_what_it_rests_on() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut members = Store::created_by(alice);
    members.invite(alice, 1, bob);
    members.invite(alice, 1, carol);
    members.act(EventKind::Removed, carol, 2, alice, 1);
    let mut tombstone = members.clone();
    let bob_promoted = members.act(EventKind::Promoted, bob, 2, alice, 1);
    let bob_demoted = members.act(EventKind::Demoted, bob, 3, alice, 1);
    let rejoined = members.join(bob, 2, carol, 3);

    let rests = rests_of(&tombstone, carol);
    assert!(rests.admits(&key_of(&members, rejoined)));
    assert!(!rests.admits(&key_of(&members, bob_promoted)));

    tombstone.entries.push(members.entries[rejoined].clone());
    let rests = rests_of(&tombstone, carol);
    assert!(rests.admits(&key_of(&members, bob_promoted)));
    assert!(!rests.admits(&key_of(&members, bob_demoted)));
}
