//! A departure's past: the entries of a membership store a former member's
//! departure depends on, which is all a member device serves it and takes
//! from it, and the points of the chains it rests on, at which its
//! tombstone takes every event — by the pod stores spec's requirement on a
//! departed member's tombstone.

use std::collections::{BTreeMap, HashMap, HashSet};

use pdn_store::AuthorId;
use pdn_types::PdnId;

use super::{
    keys::{EventKind, MembershipKey},
    membership_view::{HeldEntry, MembershipView, Verdict},
    payloads::DevicesPayload,
};

/// An entry of the past, as a session's filters name it.
pub(crate) type PastEntry = (Vec<u8>, AuthorId);

/// The past of `member`'s departure over `entries`, the ones `membership`
/// was built from; `None` while the membership view shows no departure of
/// `member` ([`Member::departed_at`](super::Member::departed_at)), whatever
/// its chain holds after one.
pub(crate) fn departure_past(
    membership: &MembershipView,
    entries: &[HeldEntry],
    member: &PdnId,
) -> Option<HashSet<PastEntry>> {
    let seq = membership.member(member)?.departed_at()?;
    let index = Index::of(membership, entries);
    let at = index.events.get(&(*member, seq))?;
    let kind = [EventKind::Removed, EventKind::Left]
        .into_iter()
        .find(|kind| at.iter().any(|event| event.kind == *kind && event.counted))?;
    let mut past = HashSet::new();
    let mut work: Vec<usize> = at
        .iter()
        .filter(|event| event.kind == kind && event.counted)
        .map(|event| event.entry)
        .collect();
    while let Some(entry) = work.pop() {
        if !past.insert(entry) {
            continue;
        }
        work.extend(index.depends_on(entry));
    }
    Some(
        past.into_iter()
            .filter_map(|entry| entries.get(entry))
            .map(|held| (held.key.clone(), held.author))
            .collect(),
    )
}

/// What a replica whose own identity departed takes from a member device:
/// its own chain whole, every event at each point of another chain that an
/// event it takes names as its actor's — held here or not, so an event that
/// reverses the departure reaches it — and the device statements of each
/// such actor. Read from the event keys alone, so each session reaches one
/// layer of dependencies further than the last.
#[derive(Debug, Default)]
pub(crate) struct Rests {
    /// Per chain, the highest sequence taken, with every one below it.
    upto: HashMap<PdnId, u64>,
    statements_of: HashSet<PdnId>,
}

impl Rests {
    /// Over the `entries` the replica holds, `member` being its identity.
    pub(crate) fn of(entries: &[HeldEntry], member: &PdnId) -> Self {
        let mut actors: HashMap<PdnId, BTreeMap<u64, Vec<(PdnId, u64)>>> = HashMap::new();
        for held in entries {
            if let Some(MembershipKey::Event {
                subject,
                seq,
                actor,
                actor_seq,
                ..
            }) = MembershipKey::parse(&held.key)
            {
                actors
                    .entry(subject)
                    .or_default()
                    .entry(seq.get())
                    .or_default()
                    .push((actor, actor_seq.get()));
            }
        }
        let mut rests = Self::default();
        rests.statements_of.insert(*member);
        let mut work = vec![(*member, u64::MAX)];
        while let Some((chain, upto)) = work.pop() {
            let from = rests.upto.get(&chain).copied().unwrap_or(0);
            if upto <= from {
                continue;
            }
            rests.upto.insert(chain, upto);
            let Some(held) = actors.get(&chain) else {
                continue;
            };
            for (actor, actor_seq) in held
                .range(from.saturating_add(1)..=upto)
                .flat_map(|(_seq, events)| events)
            {
                rests.statements_of.insert(*actor);
                work.push((*actor, *actor_seq));
            }
        }
        rests
    }

    pub(crate) fn admits(&self, key: &[u8]) -> bool {
        match MembershipKey::parse(key) {
            Some(MembershipKey::Event { subject, seq, .. }) => self
                .upto
                .get(&subject)
                .is_some_and(|upto| seq.get() <= *upto),
            Some(MembershipKey::Devices { member, .. }) => self.statements_of.contains(&member),
            None => false,
        }
    }
}

#[derive(Clone, Copy)]
struct Event {
    entry: usize,
    subject: PdnId,
    seq: u64,
    kind: EventKind,
    actor: PdnId,
    actor_seq: u64,
    counted: bool,
}

/// The store's entries by what each depends on.
struct Index<'a> {
    entries: &'a [HeldEntry],
    /// Every event entry at each sequence of each chain, whatever it counts
    /// for: a sequence is held once any entry sits at it.
    events: HashMap<(PdnId, u64), Vec<Event>>,
    by_entry: HashMap<usize, Event>,
    /// Each member's counted statements by version.
    statements: HashMap<PdnId, BTreeMap<u64, Vec<usize>>>,
    runs: HashMap<PdnId, u64>,
}

impl<'a> Index<'a> {
    fn of(membership: &MembershipView, entries: &'a [HeldEntry]) -> Self {
        let mut index = Self {
            entries,
            events: HashMap::new(),
            by_entry: HashMap::new(),
            statements: HashMap::new(),
            runs: HashMap::new(),
        };
        let verdicts = membership.verdicts();
        for (entry, held) in entries.iter().enumerate() {
            let counted = verdicts.get(entry) == Some(&Verdict::Counted);
            match MembershipKey::parse(&held.key) {
                Some(MembershipKey::Event {
                    subject,
                    seq,
                    kind,
                    actor,
                    actor_seq,
                }) => {
                    let event = Event {
                        entry,
                        subject,
                        seq: seq.get(),
                        kind,
                        actor,
                        actor_seq: actor_seq.get(),
                        counted,
                    };
                    index
                        .events
                        .entry((subject, seq.get()))
                        .or_default()
                        .push(event);
                    index.by_entry.insert(entry, event);
                }
                Some(MembershipKey::Devices { member, version }) if counted => {
                    index
                        .statements
                        .entry(member)
                        .or_default()
                        .entry(version)
                        .or_default()
                        .push(entry);
                }
                _ => {}
            }
        }
        let chains: HashSet<PdnId> = index.events.keys().map(|(member, _seq)| *member).collect();
        for member in chains {
            let mut run = 0;
            while index.events.contains_key(&(member, run + 1)) {
                run += 1;
            }
            index.runs.insert(member, run);
        }
        index
    }

    fn run(&self, member: &PdnId) -> u64 {
        self.runs.get(member).copied().unwrap_or(0)
    }

    fn chain_upto(&self, member: PdnId, upto: u64) -> impl Iterator<Item = usize> + '_ {
        (1..=upto).flat_map(move |seq| {
            self.events
                .get(&(member, seq))
                .into_iter()
                .flatten()
                .map(|event| event.entry)
        })
    }

    /// The event of `member`'s chain carrying its identity key: its
    /// counted created or joined event of the lowest sequence.
    fn key_event(&self, member: PdnId) -> Option<usize> {
        (1..=self.run(&member)).find_map(|seq| {
            self.events.get(&(member, seq))?.iter().find_map(|event| {
                (event.counted && matches!(event.kind, EventKind::Created | EventKind::Joined))
                    .then_some(event.entry)
            })
        })
    }

    /// `member`'s counted statements of the lowest version listing `author`.
    fn statements_listing(&self, member: PdnId, author: AuthorId) -> Vec<usize> {
        let Some(versions) = self.statements.get(&member) else {
            return Vec::new();
        };
        for at_version in versions.values() {
            let listing: Vec<usize> = at_version
                .iter()
                .copied()
                .filter(|entry| {
                    self.entries
                        .get(*entry)
                        .and_then(|held| held.payload.bytes())
                        .and_then(DevicesPayload::decode)
                        .is_some_and(|payload| {
                            payload.devices.iter().any(|device| device.author == author)
                        })
                })
                .collect();
            if !listing.is_empty() {
                return listing;
            }
        }
        Vec::new()
    }

    fn depends_on(&self, entry: usize) -> Vec<usize> {
        let Some(held) = self.entries.get(entry) else {
            return Vec::new();
        };
        if let Some(event) = self.by_entry.get(&entry) {
            let mut deps: Vec<usize> = self
                .chain_upto(event.subject, event.seq.saturating_sub(1))
                .chain(self.chain_upto(event.actor, event.actor_seq))
                .collect();
            deps.extend(self.statements_listing(event.actor, held.author));
            deps.extend(self.key_event(event.actor));
            return deps;
        }
        match MembershipKey::parse(&held.key) {
            Some(MembershipKey::Devices { member, .. }) => {
                self.key_event(member).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
