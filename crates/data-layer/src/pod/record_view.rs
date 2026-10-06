//! The record view: what the entries a device holds in a pod's record
//! store read as, each judged on its own against the membership — by the
//! pod stores spec's requirements on the record store.

use std::collections::BTreeMap;

use iroh_blobs::Hash;
use pdn_store::AuthorId;
use pdn_types::{RecordKind, RecordRef};

use super::{
    fold::{Awaiting, ForNothing, Membership, Verdict},
    keys::{OpId, RecordKey},
};

/// One entry of a record store as a device holds it; `payload` is its
/// content hash once the bytes have arrived.
#[derive(Clone, Debug)]
pub struct RecordEntry {
    pub key: Vec<u8>,
    pub author: AuthorId,
    pub timestamp: u64,
    pub payload: Option<Hash>,
}

/// A mergeable-document's operation that reads, with its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub id: OpId,
    pub payload: Vec<u8>,
}

/// What a device's record-store entries read as under one membership.
#[derive(Clone, Debug)]
pub struct RecordView {
    entries: Vec<RecordEntry>,
    verdicts: Vec<Verdict>,
    /// Per record, the entries of it that read.
    read: BTreeMap<RecordRef, Vec<usize>>,
}

impl RecordView {
    pub fn new(membership: &Membership, entries: Vec<RecordEntry>) -> Self {
        let mut read: BTreeMap<RecordRef, Vec<usize>> = BTreeMap::new();
        let mut verdicts = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let verdict = match RecordKey::parse(&entry.key) {
                Some(key) => {
                    let verdict = judge(membership, &key, entry);
                    if verdict == Verdict::Counted {
                        read.entry(key.record()).or_default().push(index);
                    }
                    verdict
                }
                None => Verdict::OutsideLayout,
            };
            verdicts.push(verdict);
        }
        Self {
            entries,
            verdicts,
            read,
        }
    }

    /// Every record at least one entry of which reads.
    pub fn records(&self) -> impl Iterator<Item = &RecordRef> {
        self.read.keys()
    }

    /// A claim's or an immutable-document's entry that reads, the newest by
    /// timestamp where several do; `None` for a mergeable-document.
    pub fn placed(&self, record: &RecordRef) -> Option<&RecordEntry> {
        if record.kind == RecordKind::MergeableDocument {
            return None;
        }
        self.read_entries(record)
            .max_by(|a, b| (a.timestamp, &a.key, a.author).cmp(&(b.timestamp, &b.key, b.author)))
    }

    /// A mergeable-document's operations that read, in the order of their
    /// ids.
    pub fn operations(&self, record: &RecordRef) -> Vec<(OpId, &RecordEntry)> {
        let mut operations: Vec<(OpId, &RecordEntry)> = self
            .read_entries(record)
            .filter_map(|entry| match RecordKey::parse(&entry.key) {
                Some(RecordKey::Operation { op, .. }) => Some((op, entry)),
                _ => None,
            })
            .collect();
        operations.sort_by_key(|(op, _entry)| *op);
        operations
    }

    /// The operation sequence `author`'s next operation on `record` takes:
    /// one above the highest held under it, whatever the entries count for,
    /// so the count continues across a restart. `None` past `u64::MAX`.
    pub fn next_op_seq(&self, record: &RecordRef, author: AuthorId) -> Option<u64> {
        self.entries
            .iter()
            .filter(|entry| entry.author == author)
            .filter_map(|entry| match RecordKey::parse(&entry.key) {
                Some(key @ RecordKey::Operation { op, .. }) if key.record() == *record => {
                    Some(op.op_seq)
                }
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .checked_add(1)
    }

    /// Every held entry beside its verdict, in the order given.
    pub fn verdicts(&self) -> impl Iterator<Item = (&RecordEntry, Verdict)> {
        self.entries.iter().zip(self.verdicts.iter().copied())
    }

    fn read_entries(&self, record: &RecordRef) -> impl Iterator<Item = &RecordEntry> {
        self.read
            .get(record)
            .into_iter()
            .flatten()
            .filter_map(|index| self.entries.get(*index))
    }
}

/// The writer's chain is checked first: an entry ahead of its writer's
/// events waits for them rather than counting for nothing.
fn judge(membership: &Membership, key: &RecordKey, entry: &RecordEntry) -> Verdict {
    let (writer, mseq) = match *key {
        RecordKey::Claim { member, mseq, .. }
        | RecordKey::ImmutableDocument { member, mseq, .. } => (member, mseq),
        RecordKey::Operation { op, .. } => {
            if op.author != entry.author {
                return Verdict::CountedForNothing(ForNothing::AuthorNotNamed);
            }
            (op.writer, op.mseq)
        }
    };
    let Some(member) = membership.member(&writer) else {
        return Verdict::NotYet(Awaiting::ActorChain);
    };
    let Some(state) = member.state_at(mseq.get()) else {
        return Verdict::NotYet(Awaiting::ActorChain);
    };
    if !member
        .devices
        .iter()
        .any(|device| device.author == entry.author)
    {
        return Verdict::CountedForNothing(ForNothing::AuthorNotActorDevice);
    }
    if !state.member {
        return Verdict::CountedForNothing(ForNothing::ActorLacksState);
    }
    if entry.payload.is_none() {
        return Verdict::NotYet(Awaiting::Payload);
    }
    Verdict::Counted
}

#[cfg(test)]
mod tests;
