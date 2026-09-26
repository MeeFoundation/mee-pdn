//! The private metadata store: the one device-replicated directory of an
//! identity's own state, device-internal by ticket alone (Invariant 1).
//! Five record families under disjoint prefixes: `devices/`,
//! `pending-devices/`, `tickets/`, `connections/`, `retractions/`. Device
//! and connection records are record-level; ticket and marker payloads are
//! blobs, so their reads wait for content.

use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use futures_core::Stream;
use futures_lite::StreamExt;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    engine::LiveEvent,
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use pdn_types::{NodeId, PdnId};
use serde::{Deserialize, Serialize};

use crate::node::{read_payload, SyncNode};

/// The wait of [`CatchUpWatch::wait`] elapsed. Downcast
/// from its `anyhow::Error` to tell "did not catch up in time" from this
/// node's own failures.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no successful sync session of the replica within the wait")]
pub struct CatchUpTimeout;

/// A subscription to one replica's sync sessions, from
/// [`PrivateMetadataStore::watch_catch_up`]. Unread past its buffer it drops
/// events, a session's among them, and the wait then holds out for the next
/// session — one reconcile interval at most.
pub struct CatchUpWatch {
    events: Pin<Box<dyn Stream<Item = Result<LiveEvent>> + Send>>,
    since: SystemTime,
}

impl std::fmt::Debug for CatchUpWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatchUpWatch")
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

impl CatchUpWatch {
    /// Wait for the first successful sync session started since the watch
    /// was taken, or fail with [`CatchUpTimeout`]. A completed session, not
    /// arrived content: a replica that synced and found nothing new and one
    /// that never synced read the same.
    pub async fn wait(mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CatchUpTimeout.into());
            }
            let Ok(event) = tokio::time::timeout(remaining, self.events.next()).await else {
                return Err(CatchUpTimeout.into());
            };
            let event =
                event.context("replica event stream ended while waiting for a sync session")??;
            if let LiveEvent::SyncFinished(sync) = event {
                if sync.result.is_ok() && sync.started >= self.since {
                    return Ok(());
                }
            }
        }
    }
}

/// The one key shape shared by the directory's device set, the connection
/// metadata store's published device sets, and the access book's probe:
/// it decides who counts as an identity's own device, so a drifted copy
/// would be an access-control bug. A record here is a *confirmed* device,
/// one holding this directory's write ticket.
pub(crate) const DEVICES_PREFIX: &str = "devices/";
/// Disjoint from [`DEVICES_PREFIX`] on purpose: the access book probes that
/// prefix alone, so a pending record grants nothing until the newcomer
/// confirms itself.
const PENDING_DEVICES_PREFIX: &str = "pending-devices/";
const PENDING_DEVICE_MARKER_VERSION: u8 = 1;
pub const PENDING_DEVICE_TTL: Duration = Duration::from_hours(24);
const TICKETS_PREFIX: &str = "tickets/";
const CONNECTIONS_PREFIX: &str = "connections/";
const RETRACTIONS_PREFIX: &str = "retractions/";

pub(crate) fn device_key(device: &NodeId) -> String {
    format!("{DEVICES_PREFIX}{device}")
}

fn pending_device_key(device: &NodeId) -> String {
    format!("{PENDING_DEVICES_PREFIX}{device}")
}

fn ticket_key(kind: &str) -> String {
    format!("{TICKETS_PREFIX}{kind}")
}

fn connection_key(peer: &PdnId) -> String {
    format!("{CONNECTIONS_PREFIX}{peer}")
}

pub(crate) fn device_of(key: &[u8]) -> Option<NodeId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(DEVICES_PREFIX)?
        .parse()
        .ok()
}

fn pending_device_of(key: &[u8]) -> Option<NodeId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(PENDING_DEVICES_PREFIX)?
        .parse()
        .ok()
}

enum PendingMarker {
    Legacy,
    Timestamp(SystemTime),
    UnknownVersion,
    Malformed,
}

fn pending_device_marker(marker: &[u8]) -> PendingMarker {
    if marker == [PENDING_DEVICE_MARKER_VERSION] {
        return PendingMarker::Legacy;
    }
    let Some((&version, timestamp)) = marker.split_first() else {
        return PendingMarker::Malformed;
    };
    if version != PENDING_DEVICE_MARKER_VERSION {
        return PendingMarker::UnknownVersion;
    }
    let Ok(timestamp) = <[u8; 8]>::try_from(timestamp) else {
        return PendingMarker::Malformed;
    };
    UNIX_EPOCH
        .checked_add(Duration::from_secs(u64::from_be_bytes(timestamp)))
        .map_or(PendingMarker::Malformed, PendingMarker::Timestamp)
}

fn connection_peer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(CONNECTIONS_PREFIX)?
        .parse()
        .ok()
}

/// `retractions/<issuer-hex>/<author-hex>/<path>`.
fn retraction_key(issuer: &PdnId, author: &AuthorId, path: &str) -> String {
    format!("{RETRACTIONS_PREFIX}{issuer}/{author}/{path}")
}

fn retraction_of(key: &[u8]) -> Option<(PdnId, AuthorId, String)> {
    let rest = std::str::from_utf8(key)
        .ok()?
        .strip_prefix(RETRACTIONS_PREFIX)?;
    let (issuer, rest) = rest.split_once('/')?;
    let (author, path) = rest.split_once('/')?;
    if path.is_empty() {
        return None;
    }
    Some((issuer.parse().ok()?, author.parse().ok()?, path.to_owned()))
}

/// One listed marker: the addressed entry (issuer, author, path) and the
/// decoded marker.
pub type ListedRetraction = (PdnId, AuthorId, String, RetractionMarker);

/// The payload of a retraction marker; the addressed entry lives in the
/// key. JSON; a payload this build cannot decode reads as no marker
/// (fail-closed: nothing is removed on its word).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractionMarker {
    /// Entries of the marker's author and path with a timestamp at or below
    /// this are retracted.
    pub bound: u64,
    pub decided_by: NodeId,
    /// The address of what was lost, recoverable from the blob store while
    /// the blob lives.
    pub content_hash: [u8; 32],
    pub timestamp: u64,
}

/// The owning identity is not kept here — the handle's identity knows which
/// identity it serves.
#[derive(Debug)]
pub struct PrivateMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
    pending_mutations: Arc<tokio::sync::Mutex<()>>,
}

impl PrivateMetadataStore {
    pub async fn create(node: &SyncNode, identity: PdnId) -> Result<Self> {
        // Author first, tracked doc last: nothing awaits between the
        // tracking and the handle reaching the caller, so a dropped future
        // cannot leave a tracked replica no handle refers to. The
        // identity's one author: per-store authors would leave a record
        // written before a restart standing beside its replacement written
        // after one.
        let author = node.default_author(identity)?;
        let doc = node.new_doc(identity).await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
            pending_mutations: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Import via the write ticket the linking reply carries.
    pub async fn import(node: &SyncNode, identity: PdnId, ticket: DocTicket) -> Result<Self> {
        // Author first, tracked doc last — see `create`.
        let author = node.default_author(identity)?;
        let doc = node.import_doc(identity, ticket).await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
            pending_mutations: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Recovery's constructor: open a replica this node already holds. A
    /// namespace the store does not hold is `Ok(None)`, so a caller acting on
    /// a durable record tells an absent replica from a store that failed to
    /// answer.
    pub async fn open(
        node: &SyncNode,
        identity: PdnId,
        namespace: NamespaceId,
    ) -> Result<Option<Self>> {
        let author = node.default_author(identity)?;
        let Some(doc) = node.open_doc(identity, namespace).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            doc,
            author,
            blobs: node.blobs(),
            pending_mutations: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }

    pub async fn share_ticket(
        &self,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// For registration with the node's access book.
    pub(crate) fn doc_handle(&self) -> Doc {
        self.doc.clone()
    }

    /// Record `device` as a confirmed device — one holding this directory's
    /// write ticket.
    pub async fn add_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .set_bytes(self.author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Record that `device` has begun linking, conferring nothing; the
    /// newcomer promotes itself with [`confirm_device`](Self::confirm_device).
    pub async fn add_pending_device(&self, device: NodeId) -> Result<()> {
        let _mutation = self.pending_mutations.lock().await;
        self.write_pending_device_at(device, SystemTime::now())
            .await
    }

    async fn write_pending_device_at(&self, device: NodeId, created_at: SystemTime) -> Result<()> {
        let created_at = created_at
            .duration_since(UNIX_EPOCH)
            .context("pending-device creation time predates the Unix epoch")?
            .as_secs();
        let mut marker = Vec::with_capacity(9);
        marker.push(PENDING_DEVICE_MARKER_VERSION);
        marker.extend_from_slice(&created_at.to_be_bytes());
        self.doc
            .set_bytes(
                self.author,
                pending_device_key(&device).into_bytes(),
                marker,
            )
            .await?;
        Ok(())
    }

    #[cfg(feature = "test-util")]
    pub async fn add_pending_device_at_for_test(
        &self,
        device: NodeId,
        created_at: SystemTime,
    ) -> Result<()> {
        let _mutation = self.pending_mutations.lock().await;
        self.write_pending_device_at(device, created_at).await
    }

    pub async fn cleanup_pending_devices(&self) -> Result<()> {
        self.cleanup_pending_devices_at(SystemTime::now()).await
    }

    async fn cleanup_pending_devices_at(&self, now: SystemTime) -> Result<()> {
        let _mutation = self.pending_mutations.lock().await;
        let query = Query::single_latest_per_key().key_prefix(PENDING_DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut records = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            if let Some(device) = pending_device_of(entry.key()) {
                records.push(device);
            }
        }
        for device in records {
            let key = pending_device_key(&device);
            let Some(marker) = read_payload(&self.doc, &self.blobs, key.as_bytes()).await? else {
                continue;
            };
            let created_at = match pending_device_marker(&marker) {
                PendingMarker::Legacy => {
                    self.write_pending_device_at(device, now).await?;
                    continue;
                }
                PendingMarker::Timestamp(created_at) => created_at,
                PendingMarker::UnknownVersion => continue,
                PendingMarker::Malformed => {
                    self.doc.del(self.author, key.into_bytes()).await?;
                    continue;
                }
            };
            if now.duration_since(created_at).unwrap_or_default() >= PENDING_DEVICE_TTL {
                self.doc.del(self.author, key.into_bytes()).await?;
            }
        }
        Ok(())
    }

    #[cfg(feature = "test-util")]
    pub async fn cleanup_pending_devices_at_for_test(&self, now: SystemTime) -> Result<()> {
        self.cleanup_pending_devices_at(now).await
    }

    /// Write `device`'s record under `author` rather than the node's own —
    /// the negative control for
    /// [`live_device_record_count`](Self::live_device_record_count).
    #[cfg(feature = "test-util")]
    pub async fn add_device_as_for_test(&self, device: NodeId, author: AuthorId) -> Result<()> {
        self.doc
            .set_bytes(author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Live records at `device`'s key across authors — what every product
    /// read collapses latest-wins, so this is the only way to assert one
    /// author per hosted identity.
    #[cfg(feature = "test-util")]
    pub async fn live_device_record_count(&self, device: NodeId) -> Result<usize> {
        let query = Query::all().key_exact(device_key(&device).into_bytes());
        let mut entries = std::pin::pin!(self.doc.get_many(query).await?);
        let mut count = 0usize;
        while let Some(entry) = entries.next().await {
            let _live = entry?;
            count += 1;
        }
        Ok(count)
    }

    /// Promote `device` from pending to confirmed. Written by the newcomer
    /// itself: only a identity of the write ticket can, so the record is
    /// evidence the linking reply arrived — which the inviter cannot
    /// establish on its own.
    pub async fn confirm_device(&self, device: NodeId) -> Result<()> {
        let _mutation = self.pending_mutations.lock().await;
        self.doc
            .del(self.author, pending_device_key(&device).into_bytes())
            .await?;
        self.add_device(device).await?;
        Ok(())
    }

    /// The confirmed devices, record-level.
    pub async fn list_devices(&self) -> Result<Vec<NodeId>> {
        let query = Query::single_latest_per_key().key_prefix(DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut devices = HashSet::new();
        while let Some(entry) = stream.next().await {
            if let Some(device) = device_of(entry?.key()) {
                devices.insert(device);
            }
        }
        Ok(devices.into_iter().collect())
    }

    /// The devices that began linking and have not confirmed, after a
    /// cleanup pass.
    pub async fn list_pending_devices(&self) -> Result<Vec<NodeId>> {
        self.cleanup_pending_devices().await?;
        let query = Query::single_latest_per_key().key_prefix(PENDING_DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut devices = HashSet::new();
        while let Some(entry) = stream.next().await {
            if let Some(device) = pending_device_of(entry?.key()) {
                devices.insert(device);
            }
        }
        Ok(devices.into_iter().collect())
    }

    /// Record a live connection to `peer`; the payload is an opaque marker.
    pub async fn connect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .set_bytes(self.author, connection_key(&peer).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Tombstone the connection to `peer`.
    pub async fn disconnect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, connection_key(&peer).into_bytes())
            .await?;
        Ok(())
    }

    /// Record-level: `true` as soon as the connect entry is present; a
    /// tombstone reads as not connected.
    pub async fn is_connected(&self, peer: PdnId) -> Result<bool> {
        let query = Query::single_latest_per_key().key_exact(connection_key(&peer).as_bytes());
        Ok(self.doc.get_one(query).await?.is_some())
    }

    /// Live connections, record-level.
    pub async fn list_connections(&self) -> Result<Vec<PdnId>> {
        let query = Query::single_latest_per_key().key_prefix(CONNECTIONS_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut peers = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(peer) = connection_peer_of(entry?.key()) {
                peers.push(peer);
            }
        }
        Ok(peers)
    }

    /// Record a write-retraction verdict, replacing any previous marker for
    /// the same entry address — the widest bound wins at the consumer, so a
    /// replacement never un-retracts.
    pub async fn record_retraction(
        &self,
        issuer: PdnId,
        author: AuthorId,
        path: &str,
        marker: &RetractionMarker,
    ) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                retraction_key(&issuer, &author, path).into_bytes(),
                serde_json::to_vec(marker)?,
            )
            .await?;
        Ok(())
    }

    /// The recorded markers whose payload is readable; an undecodable
    /// payload is skipped (fail-closed).
    pub async fn list_retractions(&self) -> Result<Vec<ListedRetraction>> {
        let query = Query::single_latest_per_key().key_prefix(RETRACTIONS_PREFIX.as_bytes());
        let mut keys = Vec::new();
        {
            let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
            while let Some(entry) = stream.next().await {
                if let Some(parsed) = retraction_of(entry?.key()) {
                    keys.push(parsed);
                }
            }
        }
        let mut markers = Vec::new();
        for (issuer, author, path) in keys {
            let key = retraction_key(&issuer, &author, &path);
            let Some(bytes) = read_payload(&self.doc, &self.blobs, key.as_bytes()).await? else {
                continue;
            };
            let Ok(marker) = serde_json::from_slice::<RetractionMarker>(&bytes) else {
                continue;
            };
            markers.push((issuer, author, path, marker));
        }
        Ok(markers)
    }

    /// Drop every marker this device recorded for `issuer` — with the
    /// granted namespace binding. Only own-author markers, since deletion is
    /// per directory author; each sibling prunes its own at its unbind.
    pub async fn prune_retractions(&self, issuer: PdnId) -> Result<()> {
        let query = Query::author(self.author)
            .key_prefix(format!("{RETRACTIONS_PREFIX}{issuer}/").into_bytes());
        let mut keys = Vec::new();
        {
            let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
            while let Some(entry) = stream.next().await {
                keys.push(entry?.key().to_vec());
            }
        }
        for key in keys {
            self.doc.del(self.author, key).await?;
        }
        Ok(())
    }

    /// Drop the markers this device recorded whose entry aged past
    /// `retention` (microseconds, like entry timestamps). Only own-author
    /// markers, since deletion is per directory author. Returns the dropped
    /// addresses so the caller can disarm what each one armed.
    pub async fn prune_aged_retractions(
        &self,
        now: u64,
        retention: u64,
    ) -> Result<Vec<(PdnId, AuthorId, String)>> {
        let cutoff = now.saturating_sub(retention);
        let query = Query::single_latest_per_key().key_prefix(RETRACTIONS_PREFIX.as_bytes());
        let mut aged = Vec::new();
        {
            let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
            while let Some(entry) = stream.next().await {
                let entry = entry?;
                if entry.author() == self.author && entry.timestamp() <= cutoff {
                    aged.push(entry.key().to_vec());
                }
            }
        }
        let mut dropped = Vec::new();
        for key in aged {
            if let Some(address) = retraction_of(&key) {
                dropped.push(address);
            }
            self.doc.del(self.author, key).await?;
        }
        Ok(dropped)
    }

    pub async fn put_ticket(&self, kind: &str, ticket: &DocTicket) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                ticket_key(kind).into_bytes(),
                ticket.to_string().into_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Crate-private: the fork's event type stays behind this layer.
    pub(crate) async fn events(
        &self,
    ) -> Result<impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static> {
        self.doc.subscribe().await
    }

    /// A detail-free item after every observed change — an entry written
    /// here, arrived by sync, or a payload become readable; a burst past the
    /// subscription's buffer arrives as one item. An `Err` item is the
    /// subscription failing; the stream ends with the node.
    pub async fn changes(&self) -> Result<impl Stream<Item = Result<()>> + Send + Unpin + 'static> {
        let events = self.events().await?;
        Ok(events.filter_map(|event| match event {
            Ok(
                LiveEvent::InsertLocal { .. }
                | LiveEvent::InsertRemote { .. }
                | LiveEvent::ContentReady { .. }
                | LiveEvent::Lagged,
            ) => Some(Ok(())),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }))
    }

    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
    }

    /// Taken before whatever starts the replica's sessions — before
    /// `host_identity` arms a directory — or a session finished before the
    /// subscription goes unseen and the wait holds out for the next one.
    pub async fn watch_catch_up(&self) -> Result<CatchUpWatch> {
        let since = SystemTime::now();
        let events = self.events().await?;
        Ok(CatchUpWatch {
            events: Box::pin(events),
            since,
        })
    }

    /// The kinds under which tickets are published, record-level.
    pub async fn list_ticket_kinds(&self) -> Result<Vec<String>> {
        let query = Query::single_latest_per_key().key_prefix(TICKETS_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut kinds = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            let Ok(key) = std::str::from_utf8(entry.key()) else {
                continue;
            };
            if let Some(kind) = key.strip_prefix(TICKETS_PREFIX) {
                kinds.push(kind.to_owned());
            }
        }
        Ok(kinds)
    }

    /// `Ok(None)` while the payload is still syncing. A payload that does
    /// not decode is an error, not an absence: only the identity's own
    /// devices write here, so garbage is this implementation's own bug —
    /// unlike the counterparty-written grants of
    /// [`ConnectionMetadataStore::read_grant`](crate::ConnectionMetadataStore::read_grant).
    pub async fn get_ticket(&self, kind: &str) -> Result<Option<DocTicket>> {
        let Some(bytes) = read_payload(&self.doc, &self.blobs, ticket_key(kind).as_bytes()).await?
        else {
            return Ok(None);
        };
        let ticket = std::str::from_utf8(&bytes)?.parse::<DocTicket>()?;
        Ok(Some(ticket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_marker_rejects_overflow_and_preserves_unknown_versions() {
        let mut overflow = vec![PENDING_DEVICE_MARKER_VERSION];
        overflow.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(
            pending_device_marker(&overflow),
            PendingMarker::Malformed
        ));
        assert!(matches!(
            pending_device_marker(&[PENDING_DEVICE_MARKER_VERSION + 1, 0]),
            PendingMarker::UnknownVersion
        ));
    }
}
