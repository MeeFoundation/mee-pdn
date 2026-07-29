//! Writer-side retraction gate: the trust half of write retraction.
//! Acceptance of a foreign write is locally unobservable — an accepted and a
//! refused write leave the writer's replica identical — so the issuer's gate
//! signals a capability refusal back in-band ([`pdn_store::RejectId`], echoed
//! on the reconciliation reply). This gate honors a rejection only when it
//! comes from a device of the issuer (resolved through the pair's published
//! device set) and names an own author's entry, then emits a verdict at once —
//! one session, no counting. The verdict is a name, not yet an act: what
//! makes its fields true is the local record
//! ([`SyncNode::holds_rejected_entry`](crate::SyncNode::holds_rejected_entry),
//! consulted before the runtime records anything), so a forged rejection
//! cannot make a writer discard data it holds legitimately. The runtime
//! consumes the verdicts: it records the directory marker, and the marker
//! sweep performs the removal on every device.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use iroh_blobs::Hash;
use pdn_store::{AuthorId, NamespaceId, PeerIdBytes, RejectId};
use pdn_types::NodeId;
use tokio::sync::mpsc;

/// One not-accepted verdict: the exact entry to retract, addressed by the
/// fields the marker and the event carry.
#[derive(Debug, Clone)]
pub struct RetractionVerdict {
    /// The replica the entry sits in.
    pub namespace: NamespaceId,
    /// The entry's author — a local writer author.
    pub author: AuthorId,
    /// The entry's key (a valid entry path's bytes for entries this node's
    /// writing surface produced).
    pub key: Vec<u8>,
    /// The entry's timestamp — the marker's bound.
    pub timestamp: u64,
    /// The entry's content hash — the address of what is lost.
    pub content_hash: Hash,
}

/// The gate behind the fork's rejection observer. The observer half runs on
/// the fork's sync-actor thread, so recording is synchronous and lock-brief;
/// the deposits (which namespaces the issuer's devices belong to, which
/// authors are ours) arrive from the runtime's async side.
#[derive(Debug)]
pub(crate) struct RetractionTracker {
    /// Per granted namespace: the peers that count as the issuer's devices —
    /// the only peers whose rejection is honored. A namespace absent here is
    /// not tracked.
    issuer_devices: Mutex<HashMap<NamespaceId, HashSet<NodeId>>>,
    /// The authors whose entries are this node's own writes.
    local_authors: Mutex<HashSet<AuthorId>>,
    /// Where verdicts go; the runtime takes the receiving half once.
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

    /// Track `namespace` with exactly `devices` as the issuer's device set —
    /// replacing any previous set: the published device set moves, and the
    /// newest sweep's view is the one that counts.
    pub(crate) fn track_namespace(&self, namespace: NamespaceId, devices: HashSet<NodeId>) {
        if let Ok(mut tracked) = self.issuer_devices.lock() {
            tracked.insert(namespace, devices);
        }
    }

    /// Stop honoring rejections for `namespace` — the counterpart of
    /// forgetting the granted namespace.
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

    /// The observer entry point: one in-band rejection received from `peer`
    /// for an entry in `namespace`. Honored — and turned into a verdict at
    /// once — only when `peer` resolves as a device of the issuer and the
    /// entry is of an own author; a forged rejection from any other peer, or
    /// for an entry we did not author, is ignored.
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

    /// A rejection from a device of the issuer, for an own author's entry,
    /// verdicts at once and carries the entry's id.
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

    /// A rejection from a peer that is not a device of the issuer, for an
    /// entry we did not author, or in an untracked namespace, is ignored — a
    /// forged rejection cannot make us discard our own data.
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
