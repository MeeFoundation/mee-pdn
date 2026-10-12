//! Temp tags on the payloads this engine's replicas came to reference since
//! blob collection last read them: a run whose read predates an entry would
//! otherwise sweep that entry's payload, the only copy of a local write.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, PoisonError},
};

use iroh_blobs::{api::TempTag, Hash};

/// Holds nothing until a collection first reads the replicas through
/// [`HeldPayloads::take`]: a store no collection runs over drops every tag
/// at once.
#[derive(Debug, Default)]
pub(crate) struct HeldPayloads(Mutex<Held>);

#[derive(Debug, Default)]
struct Held {
    collected: bool,
    tags: HashMap<Hash, TempTag>,
}

impl HeldPayloads {
    /// Keeps `tag` until the next read, its entry already in the store.
    pub(crate) fn hold(&self, tag: TempTag) {
        let mut held = self.lock();
        if held.collected {
            held.tags.entry(tag.hash()).or_insert(tag);
        }
    }

    /// Whether a tag on `hash` would be kept and is not held yet.
    pub(crate) fn wants(&self, hash: &Hash) -> bool {
        let held = self.lock();
        held.collected && !held.tags.contains_key(hash)
    }

    /// Every tag held so far, taken before a read of the replicas, which
    /// then sees each one's entry; the caller drops them after that read.
    pub(crate) fn take(&self) -> Vec<TempTag> {
        let mut held = self.lock();
        held.collected = true;
        held.tags.drain().map(|(_hash, tag)| tag).collect()
    }

    /// Tags taken for a read that failed, held again.
    pub(crate) fn restore(&self, tags: Vec<TempTag>) {
        let mut held = self.lock();
        for tag in tags {
            held.tags.entry(tag.hash()).or_insert(tag);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
