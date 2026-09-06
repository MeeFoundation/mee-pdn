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
use pdn_types::NodeId;
use tokio::sync::mpsc;

/// One not-accepted verdict: the exact entry to retract.
#[derive(Debug, Clone)]
pub struct RetractionVerdict {
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
    /// Per granted namespace, the peers whose rejection is honored. A
    /// namespace absent here is not tracked.
    issuer_devices: Mutex<HashMap<NamespaceId, HashSet<NodeId>>>,
    local_authors: Mutex<HashSet<AuthorId>>,
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

    /// Track `namespace` with exactly `devices` as the issuer's device set,
    /// replacing any previous set.
    pub(crate) fn track_namespace(&self, namespace: NamespaceId, devices: HashSet<NodeId>) {
        if let Ok(mut tracked) = self.issuer_devices.lock() {
            tracked.insert(namespace, devices);
        }
    }

    /// Stop honoring rejections for `namespace`.
    pub(crate) fn untrack_namespace(&self, namespace: NamespaceId) {
        if let Ok(mut tracked) = self.issuer_devices.lock() {
            tracked.remove(&namespace);
        }
    }

    /// Record `author` as one of this node's own writers.
    pub(crate) fn track_author(&self, author: AuthorId) {
        if let Ok(mut authors) = self.local_authors.lock() {
            authors.insert(author);
        }
    }

    /// The observer entry point: one in-band rejection from `peer`, turned
    /// into a verdict at once when `peer` is a tracked issuer device and the
    /// entry is of an own author; ignored otherwise.
    pub(crate) fn record_rejection(
        &self,
        namespace: NamespaceId,
        reject: &RejectId,
        peer: &PeerIdBytes,
    ) {
        let device = NodeId::from_bytes(*peer);
        {
            let Ok(tracked) = self.issuer_devices.lock() else {
                return;
            };
            let Some(devices) = tracked.get(&namespace) else {
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
            if !authors.contains(&reject.author) {
                return;
            }
        }
        // A closed channel means the runtime consumer is gone — nothing to notify.
        let _consumer_gone = self.verdicts.send(RetractionVerdict {
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

    fn fixtures() -> (NamespaceId, AuthorId, NodeId, PeerIdBytes) {
        let namespace = NamespaceSecret::from_bytes(&[7u8; 32]).id();
        let author = Author::from_bytes(&[5u8; 32]).id();
        let issuer_device = NodeId::from_bytes([9u8; 32]);
        (namespace, author, issuer_device, [9u8; 32])
    }

    #[test]
    fn a_rejection_from_an_issuer_device_verdicts() {
        let (namespace, author, issuer_device, issuer_peer) = fixtures();
        let (tracker, mut verdicts) = RetractionTracker::new();
        tracker.track_namespace(namespace, HashSet::from([issuer_device]));
        tracker.track_author(author);

        tracker.record_rejection(
            namespace,
            &reject(author, "contact/email", 42),
            &issuer_peer,
        );

        let verdict = verdicts.try_recv().expect("a verdict at once");
        assert_eq!(verdict.namespace, namespace);
        assert_eq!(verdict.author, author);
        assert_eq!(verdict.key, b"contact/email");
        assert_eq!(verdict.timestamp, 42);
        assert!(verdicts.try_recv().is_err(), "exactly one verdict");
    }

    #[test]
    fn a_forged_or_foreign_rejection_is_ignored() {
        let (namespace, author, issuer_device, issuer_peer) = fixtures();
        let other_author = Author::from_bytes(&[6u8; 32]).id();
        let stranger_peer = [1u8; 32];
        let (tracker, mut verdicts) = RetractionTracker::new();
        tracker.track_namespace(namespace, HashSet::from([issuer_device]));
        tracker.track_author(author);

        // A non-issuer peer's rejection is ignored.
        tracker.record_rejection(namespace, &reject(author, "k", 1), &stranger_peer);
        // A rejection for an entry we did not author is ignored.
        tracker.record_rejection(namespace, &reject(other_author, "k", 1), &issuer_peer);
        // A rejection in an untracked namespace is ignored.
        let untracked = NamespaceSecret::from_bytes(&[8u8; 32]).id();
        tracker.record_rejection(untracked, &reject(author, "k", 1), &issuer_peer);

        assert!(verdicts.try_recv().is_err(), "no verdict from any of them");
    }
}
