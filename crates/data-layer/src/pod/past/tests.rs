use super::*;
use crate::pod::{
    testing::{event, Cast, Person, Store},
    EventKind,
};

fn past_of(store: &Store, member: &Person) -> Option<HashSet<PastEntry>> {
    departure_past(&store.membership_view(), &store.entries, &member.id())
        .map(|departure| departure.past)
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
        (bob_joined + 1, "Bob's statement"),
        (dave_joined, "Dave's joined event"),
        (dave_joined + 1, "Dave's statement"),
    ] {
        assert!(!past.contains(&entry(&store, index)), "{name} taken in");
    }
    assert_eq!(past.len(), 4);
}

/// A leave's past holds the member's own chain and the statement resolving
/// the device it left from, and what its join rests on. Denied: nobody
/// else's entries.
#[test]
fn a_leaves_past_holds_the_members_own_chain_and_statement() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::created_by(alice);
    let bob_joined = store.invite(alice, 1, bob);
    let carol_joined = store.invite(alice, 1, carol);
    let left = store.act(EventKind::Left, carol, 2, carol, 1);
    let past = past_of(&store, carol).expect("Carol departed");
    for index in [0, 1, carol_joined, carol_joined + 1, left] {
        assert!(past.contains(&entry(&store, index)));
    }
    for index in [bob_joined, bob_joined + 1] {
        assert!(!past.contains(&entry(&store, index)));
    }
    assert_eq!(past.len(), 5);
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
