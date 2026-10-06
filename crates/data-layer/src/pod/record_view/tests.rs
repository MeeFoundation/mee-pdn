use iroh_blobs::Hash;
use pdn_types::{PdnId, RecordId};

use super::*;
use crate::pod::{
    keys::Seq,
    testing::{device, Cast, Person, Store},
    EventKind,
};

/// A pod's record store as one device holds it, each entry newer than
/// the one before and its payload arrived.
#[derive(Clone, Default)]
struct Records {
    entries: Vec<RecordEntry>,
    clock: u64,
}

impl Records {
    fn put(&mut self, key: RecordKey, author: AuthorId) -> usize {
        self.put_raw(&key.to_string(), author)
    }

    fn put_raw(&mut self, key: &str, author: AuthorId) -> usize {
        self.clock += 1;
        self.entries.push(RecordEntry {
            key: key.as_bytes().to_vec(),
            author,
            timestamp: self.clock,
            payload: Some(Hash::new(key)),
        });
        self.entries.len() - 1
    }

    fn view(&self, store: &Store) -> RecordView {
        RecordView::new(&store.fold(), self.entries.clone())
    }
}

fn claim(member: &Person, id: u8, mseq: u64) -> RecordKey {
    RecordKey::Claim {
        member: member.id(),
        id: RecordId::from_bytes([id; 16]),
        mseq: Seq::new(mseq),
    }
}

fn scan(member: &Person, id: u8, mseq: u64) -> RecordKey {
    RecordKey::ImmutableDocument {
        member: member.id(),
        id: RecordId::from_bytes([id; 16]),
        mseq: Seq::new(mseq),
    }
}

/// An operation on `member`'s mergeable-document `id` by `writer`, signed
/// by `author`, at the writer's `mseq`.
fn op(member: &Person, id: u8, writer: &Person, author: AuthorId, mseq: u64) -> RecordKey {
    RecordKey::Operation {
        member: member.id(),
        id: RecordId::from_bytes([id; 16]),
        op: OpId {
            writer: writer.id(),
            author,
            mseq: Seq::new(mseq),
            op_seq: 1,
        },
    }
}

fn verdict(view: &RecordView, entry: usize) -> Verdict {
    view.verdicts()
        .nth(entry)
        .map_or(Verdict::OutsideLayout, |(_entry, verdict)| verdict)
}

fn nothing(why: ForNothing) -> Verdict {
    Verdict::CountedForNothing(why)
}

fn writers(view: &RecordView, record: &RecordRef) -> Vec<PdnId> {
    view.operations(record)
        .into_iter()
        .map(|(op, _entry)| op.writer)
        .collect()
}

/// Operations come in the order of their ids, the writer's `PdnId` first.
fn by_id(mut writers: Vec<PdnId>) -> Vec<PdnId> {
    writers.sort();
    writers
}

/// A claim reads from a device of the member it names as its issuer. Paired
/// denial: another member's entry at that claim's key, and a claim under
/// the issuer's name at a fresh id, are held and read by nothing, the
/// issuer's claim reading unchanged although the forgery is newer.
#[test]
fn the_issuers_claim_reads_and_another_members_entry_at_its_key_does_not() {
    let cast = Cast::new();
    let (alice, bob) = (&cast.alice, &cast.bob);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let mut records = Records::default();
    let own = records.put(claim(alice, 1, 1), alice.author());
    let at_its_key = records.put(claim(alice, 1, 1), bob.author());
    let fresh = records.put(claim(alice, 2, 1), bob.author());
    let view = records.view(&store);
    assert_eq!(verdict(&view, own), Verdict::Counted);
    for forged in [at_its_key, fresh] {
        assert_eq!(
            verdict(&view, forged),
            nothing(ForNothing::AuthorNotActorDevice)
        );
    }
    let record = claim(alice, 1, 1).record();
    assert_eq!(
        view.placed(&record).map(|entry| entry.author),
        Some(alice.author())
    );
    assert_eq!(view.records().collect::<Vec<_>>(), [&record]);
}

/// An immutable-document reads from the member under whose name it sits.
/// Paired denial: an owner's entry at its key is read by nothing.
#[test]
fn an_immutable_document_reads_from_its_member_and_not_from_an_owner() {
    let cast = Cast::new();
    let (alice, bob) = (&cast.alice, &cast.bob);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let mut records = Records::default();
    let bobs = records.put(scan(bob, 1, 1), bob.author());
    let owners = records.put(scan(bob, 1, 1), alice.author());
    let view = records.view(&store);
    assert_eq!(verdict(&view, bobs), Verdict::Counted);
    assert_eq!(
        verdict(&view, owners),
        nothing(ForNothing::AuthorNotActorDevice)
    );
    assert_eq!(
        view.placed(&scan(bob, 1, 1).record())
            .map(|entry| entry.author),
        Some(bob.author())
    );
}

/// Any member's operation on another member's mergeable-document reads,
/// with its writer. Paired denial: an operation by an author no member's
/// statement lists is read by nothing.
#[test]
fn any_member_edits_another_members_mergeable_document() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    store.invite(alice, 1, carol);
    let mut records = Records::default();
    let bobs = records.put(op(bob, 1, bob, bob.author(), 1), bob.author());
    let carols = records.put(op(bob, 1, carol, carol.author(), 1), carol.author());
    let stranger = device(0xee).author;
    let no_members = records.put(op(bob, 1, carol, stranger, 1), stranger);
    let view = records.view(&store);
    for read in [bobs, carols] {
        assert_eq!(verdict(&view, read), Verdict::Counted);
    }
    assert_eq!(
        verdict(&view, no_members),
        nothing(ForNothing::AuthorNotActorDevice)
    );
    let note = op(bob, 1, bob, bob.author(), 1).record();
    assert_eq!(writers(&view, &note), by_id(vec![bob.id(), carol.id()]));
    assert_eq!(view.placed(&note).map(|entry| entry.author), None);
}

/// An operation reads as the writer its key names, although another
/// member's statement lists its author too. Denied: nothing of that writer's
/// reads as the other member's.
#[test]
fn an_operation_reads_as_the_writer_its_key_names() {
    let cast = Cast::new();
    let (alice, bob) = (&cast.alice, &cast.bob);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let claiming = store.statement(bob, 2, &[bob.device, alice.device], bob.author());
    let mut records = Records::default();
    let alices = records.put(op(bob, 1, alice, alice.author(), 1), alice.author());
    let view = records.view(&store);
    assert_eq!(store.fold().verdicts()[claiming], Verdict::Counted);
    assert_eq!(verdict(&view, alices), Verdict::Counted);
    let note = op(bob, 1, alice, alice.author(), 1).record();
    assert_eq!(writers(&view, &note), [alice.id()]);
}

/// A departed member's operation naming a sequence at which it was a member
/// reads after its kick, and after it joins again beside its new one.
/// Paired denial: its operation naming the kick's own sequence is read by
/// nothing.
#[test]
fn a_departed_members_earlier_operation_reads_and_one_naming_its_kick_does_not() {
    let cast = Cast::new();
    let (alice, carol) = (&cast.alice, &cast.carol);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, carol);
    store.act(EventKind::Kicked, carol, 2, alice, 1);
    let mut records = Records::default();
    let earlier = records.put(op(alice, 1, carol, carol.author(), 1), carol.author());
    let at_kick = records.put(op(alice, 2, carol, carol.author(), 2), carol.author());
    let claim_at_kick = records.put(claim(carol, 3, 2), carol.author());
    let view = records.view(&store);
    assert_eq!(verdict(&view, earlier), Verdict::Counted);
    for entry in [at_kick, claim_at_kick] {
        assert_eq!(verdict(&view, entry), nothing(ForNothing::ActorLacksState));
    }

    store.join(alice, 1, carol, 3);
    let rejoined = records.put(op(alice, 1, carol, carol.author(), 3), carol.author());
    let view = records.view(&store);
    for entry in [earlier, rejoined] {
        assert_eq!(verdict(&view, entry), Verdict::Counted);
    }
    assert_eq!(
        verdict(&view, at_kick),
        nothing(ForNothing::ActorLacksState)
    );
    let note = op(alice, 1, carol, carol.author(), 1).record();
    assert_eq!(writers(&view, &note), [carol.id(), carol.id()]);
}

/// An operation ahead of its writer's joined event, a claim naming a
/// sequence its writer's chain does not yet hold, and a claim ahead of its
/// payload are held and read by nothing, and each reads once what it waits
/// for arrives. Paired denial: another member's operation naming that
/// writer is read by nothing once the event arrives.
#[test]
fn an_entry_ahead_of_what_it_names_reads_once_that_arrives() {
    let cast = Cast::new();
    let (alice, bob, dave) = (&cast.alice, &cast.bob, &cast.dave);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let mut records = Records::default();
    let daves = records.put(op(alice, 1, dave, dave.author(), 1), dave.author());
    let forged = records.put(op(alice, 1, dave, bob.author(), 1), bob.author());
    let ahead_of_chain = records.put(claim(bob, 3, 2), bob.author());
    let payload_later = records.put(claim(bob, 2, 1), bob.author());
    let payload = records.entries[payload_later].payload.take();
    let view = records.view(&store);
    for waiting in [daves, forged, ahead_of_chain] {
        assert_eq!(
            verdict(&view, waiting),
            Verdict::NotYet(Awaiting::ActorChain)
        );
    }
    assert_eq!(
        verdict(&view, payload_later),
        Verdict::NotYet(Awaiting::Payload)
    );
    assert_eq!(view.records().count(), 0);

    store.invite(alice, 1, dave);
    store.act(EventKind::Promoted, bob, 2, alice, 1);
    records.entries[payload_later].payload = payload;
    let view = records.view(&store);
    for read in [daves, ahead_of_chain, payload_later] {
        assert_eq!(verdict(&view, read), Verdict::Counted);
    }
    assert_eq!(
        verdict(&view, forged),
        nothing(ForNothing::AuthorNotActorDevice)
    );
    assert_eq!(view.records().count(), 3);
}

/// An operation reads under the author its key names, the one that signs
/// it. Paired denial: another member's entry at that operation's key is
/// read by nothing, the operation reading once.
#[test]
fn an_entry_at_another_writers_operation_key_reads_nothing() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    store.invite(alice, 1, carol);
    let mut records = Records::default();
    let bobs = records.put(op(alice, 1, bob, bob.author(), 1), bob.author());
    let carols = records.put(op(alice, 1, bob, bob.author(), 1), carol.author());
    let view = records.view(&store);
    assert_eq!(verdict(&view, bobs), Verdict::Counted);
    assert_eq!(verdict(&view, carols), nothing(ForNothing::AuthorNotNamed));
    let note = op(alice, 1, bob, bob.author(), 1).record();
    assert_eq!(writers(&view, &note), [bob.id()]);
}

/// A member's claim at a sequence of its chain reads. Denied: its entries
/// naming sequence 0, before its first event, and at keys outside the
/// layout, are held and read by nothing.
#[test]
fn entries_before_a_chain_or_outside_the_layout_read_nothing() {
    let cast = Cast::new();
    let (alice, bob) = (&cast.alice, &cast.bob);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let mut records = Records::default();
    let read = records.put(claim(bob, 1, 1), bob.author());
    let before = records.put(claim(bob, 2, 0), bob.author());
    let outside = [
        records.put_raw("ext/anything", bob.author()),
        records.put_raw(&format!("by/{}/", bob.id()), bob.author()),
    ];
    let view = records.view(&store);
    assert_eq!(verdict(&view, read), Verdict::Counted);
    assert_eq!(verdict(&view, before), nothing(ForNothing::ActorLacksState));
    for entry in outside {
        assert_eq!(verdict(&view, entry), Verdict::OutsideLayout);
    }
    assert_eq!(view.records().count(), 1);
}

/// Both stores' entries taken in reverse order read the same records, with
/// the same entries and the same operations.
#[test]
fn entries_in_any_order_read_the_same() {
    let cast = Cast::new();
    let (alice, bob, carol) = (&cast.alice, &cast.bob, &cast.carol);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    store.invite(bob, 1, carol);
    let mut records = Records::default();
    records.put(claim(carol, 1, 1), carol.author());
    records.put(claim(carol, 1, 1), bob.author());
    records.put(op(alice, 2, carol, carol.author(), 1), carol.author());
    records.put(op(alice, 2, bob, bob.author(), 1), bob.author());
    let forward = records.view(&store);
    let mut reversed_store = store.clone();
    reversed_store.entries.reverse();
    let mut reversed = records.clone();
    reversed.entries.reverse();
    let backward = reversed.view(&reversed_store);
    let listed = |view: &RecordView| view.records().copied().collect::<Vec<_>>();
    assert_eq!(listed(&forward), listed(&backward));
    assert_eq!(listed(&forward).len(), 2);
    let claim = claim(carol, 1, 1).record();
    assert_eq!(
        forward.placed(&claim).map(|entry| entry.author),
        backward.placed(&claim).map(|entry| entry.author)
    );
    let note = op(alice, 2, bob, bob.author(), 1).record();
    assert_eq!(writers(&forward, &note), writers(&backward, &note));
    assert_eq!(writers(&forward, &note), by_id(vec![bob.id(), carol.id()]));
}

/// An author's next operation on a mergeable-document takes the one above
/// the highest it holds there, an operation that reads nothing yet included;
/// another author's operations and its own on another record move nothing.
#[test]
fn the_next_operation_sequence_is_one_above_the_authors_highest_held() {
    let cast = Cast::new();
    let (alice, bob) = (&cast.alice, &cast.bob);
    let mut store = Store::founded_by(alice);
    store.invite(alice, 1, bob);
    let numbered = |writer: &Person, id: u8, mseq: u64, op_seq: u64| RecordKey::Operation {
        member: bob.id(),
        id: RecordId::from_bytes([id; 16]),
        op: OpId {
            writer: writer.id(),
            author: writer.author(),
            mseq: Seq::new(mseq),
            op_seq,
        },
    };
    let note = numbered(bob, 1, 1, 1).record();
    let mut records = Records::default();
    assert_eq!(
        records.view(&store).next_op_seq(&note, bob.author()),
        Some(1)
    );
    records.put(numbered(bob, 1, 1, 1), bob.author());
    records.put(numbered(bob, 1, 1, 3), bob.author());
    let waiting = records.put(numbered(bob, 1, 5, 7), bob.author());
    records.put(numbered(alice, 1, 1, 20), alice.author());
    records.put(numbered(bob, 2, 1, 30), bob.author());
    let view = records.view(&store);
    assert_eq!(
        verdict(&view, waiting),
        Verdict::NotYet(Awaiting::ActorChain)
    );
    assert_eq!(view.next_op_seq(&note, bob.author()), Some(8));
    assert_eq!(view.next_op_seq(&note, alice.author()), Some(21));

    records.put(numbered(bob, 1, 1, u64::MAX), bob.author());
    assert_eq!(records.view(&store).next_op_seq(&note, bob.author()), None);
}
