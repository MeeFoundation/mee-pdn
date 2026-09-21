//! Writer-side retraction gate: the trust half of write retraction. An
//! in-band rejection ([`pdn_store::RejectId`]) becomes a verdict only when
//! it comes from a device of the issuer and names an own author's entry.
//! The verdict is a name, not an act: the runtime confirms it against the
//! local record ([`SyncNode::holds_rejected_entry`](crate::SyncNode::holds_rejected_entry))
//! before recording a marker, so a forged rejection cannot make a writer
//! discard data it holds legitimately.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use iroh_blobs::Hash;
use pdn_store::{AuthorId, NamespaceId, PeerIdBytes, RejectId};
use pdn_types::{NodeId, PdnId};
use tokio::sync::mpsc;

/// One not-accepted verdict: the exact entry to retract.
#[derive(Debug, Clone)]
pub struct RetractionVerdict {
    /// The hosted identity whose replica holds the entry: several
    /// identities of one node hold one namespace, each in its own replica.
    pub identity: PdnId,
    pub namespace: NamespaceId,
    /// A local writer author.
    pub author: AuthorId,
    /// The entry's key — a valid entry path's bytes for entries this node's
    /// writing surface produced.
    pub key: Vec<u8>,
    /// The marker's bound.
    pub timestamp: u64,
    /// The address of what is lost.
    pub content_hash: Hash,
}

/// The gate behind the fork's rejection observer. The observer runs on the
/// fork's sync-actor thread, so recording is synchronous and lock-brief.
#[derive(Debug)]
pub(crate) struct RetractionTracker {
    /// Per hosted identity and granted namespace, the peers whose
    /// rejection is honored. A pair absent here is not tracked. Keyed by
    /// identity because two identities of one node hold one namespace
    /// under device sets that move apart (ADR-0013).
    issuer_devices: Mutex<HashMap<(PdnId, NamespaceId), HashSet<NodeId>>>,
    /// One author per hosted identity, so a rejection naming a co-located
    /// identity's author is not this identity's to honor.
    local_authors: Mutex<HashMap<PdnId, AuthorId>>,
    /// The runtime takes the receiving half once.
    verdicts: mpsc::UnboundedSender<RetractionVerdict>,
}

impl RetractionTracker {
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<RetractionVerdict>) {
        let (verdicts, rx) = mpsc::unbounded_channel();
        (
            Self {
                issuer_devices: Mutex::default(),
                local_authors: Mutex::default(),
                verdicts,
            },
            rx,
        )
    }

    /// Track `namespace` held by `identity` with exactly `devices` as the
    /// issuer's device set, replacing any previous set.
    pub(crate) fn track_namespace(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        devices: HashSet<NodeId>,
    ) {
        if let Ok(mut tracked) = self.issuer_devices.lock() {
            tracked.insert((identity, namespace), devices);
        }
    }

    /// Stop honoring rejections for `namespace` as held by `identity`. A
    /// co-located identity's replica of the same namespace keeps its own.
    pub(crate) fn untrack_namespace(&self, identity: PdnId, namespace: NamespaceId) {
        if let Ok(mut tracked) = self.issuer_devices.lock() {
            tracked.remove(&(identity, namespace));
        }
    }

    /// Record `author` as the writer `identity` writes with.
    pub(crate) fn track_author(&self, identity: PdnId, author: AuthorId) {
        if let Ok(mut authors) = self.local_authors.lock() {
            authors.insert(identity, author);
        }
    }

    /// The observer entry point: one in-band rejection from `peer` in
    /// `identity`'s replica, turned into a verdict at once when `peer` is a
    /// tracked issuer device of that pair and the entry is of `identity`'s
    /// own author; ignored otherwise.
    pub(crate) fn record_rejection(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        reject: &RejectId,
        peer: &PeerIdBytes,
    ) {
        let device = NodeId::from_bytes(*peer);
        {
            let Ok(tracked) = self.issuer_devices.lock() else {
                return;
            };
            let Some(devices) = tracked.get(&(identity, namespace)) else {
                return;
            };
            if !devices.contains(&device) {
                return;
            }
        }
        {
            let Ok(authors) = self.local_authors.lock() else {
                return;
            };
            if authors.get(&identity) != Some(&reject.author) {
                return;
            }
        }
        // A closed channel means the runtime consumer is gone — nothing to notify.
        let _consumer_gone = self.verdicts.send(RetractionVerdict {
            identity,
            namespace,
            author: reject.author,
            key: reject.key.to_vec(),
            timestamp: reject.timestamp,
            content_hash: reject.content_hash,
        });
    }
}

#[cfg(test)]
mod tests {
    use pdn_store::{Author, NamespaceSecret};

    use super::*;

    fn reject(author: AuthorId, key: &str, timestamp: u64) -> RejectId {
        RejectId {
            author,
            key: key.as_bytes().to_vec().into(),
            timestamp,
            content_hash: Hash::new(b"payload"),
        }
    }

    fn fixtures() -> (PdnId, NamespaceId, AuthorId, NodeId, PeerIdBytes) {
        let identity = PdnId::from_bytes([3u8; 32]);
        let namespace = NamespaceSecret::from_bytes(&[7u8; 32]).id();
        let author = Author::from_bytes(&[5u8; 32]).id();
        let issuer_device = NodeId::from_bytes([9u8; 32]);
        (identity, namespace, author, issuer_device, [9u8; 32])
    }

    /// A rejection from a device of the issuer, naming an entry of the
    /// identity's own author, becomes a verdict at once.
    #[test]
    fn a_rejection_from_an_issuer_device_verdicts() {
        let (identity, namespace, author, issuer_device, issuer_peer) = fixtures();
        let (tracker, mut verdicts) = RetractionTracker::new();
        tracker.track_namespace(identity, namespace, HashSet::from([issuer_device]));
        tracker.track_author(identity, author);

        tracker.record_rejection(
            identity,
            namespace,
            &reject(author, "contact/email", 42),
            &issuer_peer,
        );

        let verdict = verdicts.try_recv().expect("a verdict at once");
        assert_eq!(verdict.identity, identity);
        assert_eq!(verdict.namespace, namespace);
        assert_eq!(verdict.author, author);
        assert_eq!(verdict.key, b"contact/email");
        assert_eq!(verdict.timestamp, 42);
        assert!(verdicts.try_recv().is_err(), "exactly one verdict");
    }

    /// A rejection from a stranger, over an entry of another author, or in
    /// a namespace this identity does not track, is ignored.
    #[test]
    fn a_forged_or_foreign_rejection_is_ignored() {
        let (identity, namespace, author, issuer_device, issuer_peer) = fixtures();
        let other_author = Author::from_bytes(&[6u8; 32]).id();
        let stranger_peer = [1u8; 32];
        let (tracker, mut verdicts) = RetractionTracker::new();
        tracker.track_namespace(identity, namespace, HashSet::from([issuer_device]));
        tracker.track_author(identity, author);

        // A non-issuer peer's rejection is ignored.
        tracker.record_rejection(identity, namespace, &reject(author, "k", 1), &stranger_peer);
        // A rejection for an entry we did not author is ignored.
        tracker.record_rejection(
            identity,
            namespace,
            &reject(other_author, "k", 1),
            &issuer_peer,
        );
        // A rejection in an untracked namespace is ignored.
        let untracked = NamespaceSecret::from_bytes(&[8u8; 32]).id();
        tracker.record_rejection(identity, untracked, &reject(author, "k", 1), &issuer_peer);

        assert!(verdicts.try_recv().is_err(), "no verdict from any of them");
    }

    /// Two identities of one node hold one namespace: what one of them
    /// tracks decides nothing for the other, in either direction.
    ///
    /// Denied: a rejection in the untracking identity's replica, from the
    /// same issuer device and naming the tracking identity's author.
    #[test]
    fn a_co_located_identity_neither_grants_nor_takes_a_verdict() {
        let (work, namespace, work_author, issuer_device, issuer_peer) = fixtures();
        let leisure = PdnId::from_bytes([4u8; 32]);
        let leisure_author = Author::from_bytes(&[6u8; 32]).id();
        let (tracker, mut verdicts) = RetractionTracker::new();
        tracker.track_namespace(work, namespace, HashSet::from([issuer_device]));
        tracker.track_author(work, work_author);
        tracker.track_author(leisure, leisure_author);

        // Denied: the pair (leisure, namespace) is untracked, whoever the
        // rejection names.
        tracker.record_rejection(
            leisure,
            namespace,
            &reject(leisure_author, "k", 1),
            &issuer_peer,
        );
        tracker.record_rejection(
            leisure,
            namespace,
            &reject(work_author, "k", 1),
            &issuer_peer,
        );
        // Denied: work tracks the pair, but the entry is the neighbour's.
        tracker.record_rejection(
            work,
            namespace,
            &reject(leisure_author, "k", 1),
            &issuer_peer,
        );
        assert!(
            verdicts.try_recv().is_err(),
            "no verdict crosses identities"
        );

        // Untracking one identity leaves the other's tracking alone.
        tracker.track_namespace(leisure, namespace, HashSet::from([issuer_device]));
        tracker.untrack_namespace(leisure, namespace);
        tracker.record_rejection(work, namespace, &reject(work_author, "k", 2), &issuer_peer);
        let verdict = verdicts
            .try_recv()
            .expect("the tracking identity still verdicts");
        assert_eq!(verdict.identity, work);
    }
}
