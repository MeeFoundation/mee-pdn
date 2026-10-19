//! The private metadata store (PMS): the one device-replicated store of an
//! identity's own state, device-internal by ticket alone (Invariant 1).
//! Four record families under disjoint prefixes: `devices/`,
//! `pending-devices/`, `tickets/` and `pods/`, and the identity key pair at
//! `identity-key`. Device and pod records are record-level; ticket and key
//! payloads are blobs, so their reads answer `None` until the content
//! arrives.

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use futures_core::Stream;
use futures_lite::StreamExt;
use pdn_store::{
    api::{AddrInfoOptions, Doc},
    engine::LiveEvent,
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use pdn_types::{NodeId, PdnId, PodId};

use crate::{
    identity_key::IdentityKeyPair,
    node::{read_payload, SyncNode},
    pod::{PodStore, Seq},
};

/// The wait of [`CatchUpWatch::wait`] elapsed, naming the sessions that
/// ended in it. Downcast from its `anyhow::Error` to tell "did not catch up
/// in time" from this node's own failures.
#[derive(Debug, Clone, Default, thiserror::Error)]
#[error("no successful sync session of the replica within the wait; ended in it: {}", ended_in_wait(.ended, *.unnamed))]
pub struct CatchUpTimeout {
    /// The first [`ENDED_NAMED`] of them.
    ended: Vec<String>,
    unnamed: usize,
}

/// Sessions a timeout names; a pair redialed as already syncing ends more.
const ENDED_NAMED: usize = 10;

fn ended_in_wait(ended: &[String], unnamed: usize) -> String {
    match (ended.is_empty(), unnamed) {
        (true, _) => "nothing".to_owned(),
        (false, 0) => ended.join("; "),
        (false, more) => format!("{} and {more} more", ended.join("; ")),
    }
}

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

/// The watch of [`PrivateMetadataStore::watch_catch_up`] over any replica.
pub(crate) async fn watch_doc(doc: &Doc) -> Result<CatchUpWatch> {
    let since = SystemTime::now();
    let events = doc.subscribe().await?;
    Ok(CatchUpWatch {
        events: Box::pin(events),
        since,
    })
}

impl CatchUpWatch {
    /// Wait for the first successful sync session started since the watch
    /// was taken, or fail with [`CatchUpTimeout`]. A completed session, not
    /// arrived content: a replica that synced and found nothing new and one
    /// that never synced read the same.
    pub async fn wait(mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut timeout_error = CatchUpTimeout::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_error.into());
            }
            let Ok(event) = tokio::time::timeout(remaining, self.events.next()).await else {
                return Err(timeout_error.into());
            };
            let event =
                event.context("replica event stream ended while waiting for a sync session")??;
            if let LiveEvent::SyncFinished(sync) = event {
                if sync.result.is_ok() && sync.started >= self.since {
                    return Ok(());
                }
                let before = if sync.started < self.since {
                    ", begun before the watch"
                } else {
                    ""
                };
                if timeout_error.ended.len() == ENDED_NAMED {
                    timeout_error.unnamed = timeout_error.unnamed.saturating_add(1);
                    continue;
                }
                timeout_error.ended.push(format!(
                    "{} as {} ({:?}{before}): {:?}",
                    sync.peer.fmt_short(),
                    sync.counterpart.fmt_short(),
                    sync.origin,
                    sync.result,
                ));
            }
        }
    }
}

/// The one key shape shared by the PMS's device set and the access book's
/// probe: it decides who counts as an identity's own device, so a drifted
/// copy would be an access-control bug. A record here is a *confirmed* device,
/// one holding this PMS's write ticket.
pub(crate) const DEVICES_PREFIX: &str = "devices/";
/// Disjoint from [`DEVICES_PREFIX`] on purpose: the access book probes that
/// prefix alone, so a pending record confers nothing until the newcomer
/// confirms itself.
const PENDING_DEVICES_PREFIX: &str = "pending-devices/";
const PENDING_DEVICE_MARKER_VERSION: u8 = 1;
pub const PENDING_DEVICE_TTL: Duration = Duration::from_hours(24);
const TICKETS_PREFIX: &str = "tickets/";
const IDENTITY_KEY_PATH: &str = "identity-key";
const PODS_PREFIX: &str = "pods/";

/// The PMS kind of one of a pod's stores' write tickets, minted by
/// the identity itself: its own devices are the nodes it names.
pub fn pod_ticket_kind(pod: &PodId, store: PodStore) -> String {
    format!("pod/{pod}/{}", pod_store_name(store))
}

/// The PMS kind of the write ticket to one of a pod's stores that
/// the device which invited the identity handed over, naming that device
/// as the inviter: a ticket names all its nodes as one identity.
pub fn pod_inviter_ticket_kind(pod: &PodId, store: PodStore) -> String {
    format!("pod/{pod}/inviter/{}", pod_store_name(store))
}

fn pod_store_name(store: PodStore) -> &'static str {
    match store {
        PodStore::Membership => "membership",
        PodStore::Records => "records",
    }
}

fn pod_record_key(pod: &PodId, seq: Seq) -> String {
    format!("{PODS_PREFIX}{pod}/{seq}")
}

fn pod_record_of(key: &[u8]) -> Option<(PodId, u64)> {
    let rest = std::str::from_utf8(key).ok()?.strip_prefix(PODS_PREFIX)?;
    let (pod, seq) = rest.split_once('/')?;
    let canonical = seq == "0" || (!seq.starts_with('0') && !seq.is_empty());
    if !canonical || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((pod.parse().ok()?, seq.parse().ok()?))
}

pub(crate) fn device_key(device: &NodeId) -> String {
    format!("{DEVICES_PREFIX}{device}")
}

fn pending_device_key(device: &NodeId) -> String {
    format!("{PENDING_DEVICES_PREFIX}{device}")
}

fn ticket_key(kind: &str) -> String {
    format!("{TICKETS_PREFIX}{kind}")
}

/// The confirmed devices a PMS replica lists, record-level.
pub(crate) async fn listed_devices(doc: &Doc) -> Result<Vec<NodeId>> {
    let query = Query::single_latest_per_key().key_prefix(DEVICES_PREFIX.as_bytes());
    let mut stream = std::pin::pin!(doc.get_many(query).await?);
    let mut devices = HashSet::new();
    while let Some(entry) = stream.next().await {
        if let Some(device) = device_of(entry?.key()) {
            devices.insert(device);
        }
    }
    Ok(devices.into_iter().collect())
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

/// The owning identity is not kept here — the caller holding the handle
/// names it.
#[derive(Debug)]
pub struct PrivateMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
    pending_mutations: Arc<tokio::sync::Mutex<()>>,
}

impl PrivateMetadataStore {
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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

    pub async fn share_ticket(&self, addr_options: AddrInfoOptions) -> Result<DocTicket> {
        let ticket = self.doc.share(addr_options).await?;
        Ok(ticket)
    }

    /// For registration with the node's access book.
    pub(crate) fn doc_handle(&self) -> Doc {
        self.doc.clone()
    }

    /// Record `device` as a confirmed device — one holding this PMS's
    /// write ticket.
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
    pub async fn add_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .set_bytes(self.author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Record that `device` has begun linking, conferring nothing; the
    /// newcomer promotes itself with [`confirm_device`](Self::confirm_device).
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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

    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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

    /// When `device`'s pending record was written, once its payload is
    /// here; `None` while the payload has not arrived or reads as no time.
    #[cfg(feature = "test-util")]
    pub async fn pending_device_written_at_for_test(
        &self,
        device: NodeId,
    ) -> Result<Option<SystemTime>> {
        let key = pending_device_key(&device);
        let marker = read_payload(&self.doc, &self.blobs, key.as_bytes()).await?;
        Ok(
            marker.and_then(|marker| match pending_device_marker(&marker) {
                PendingMarker::Timestamp(written_at) => Some(written_at),
                _ => None,
            }),
        )
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
    /// itself: only a holder of the write ticket can, so the record is
    /// evidence the linking reply arrived — which the inviter cannot
    /// establish on its own.
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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
        listed_devices(&self.doc).await
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

    /// Record that the identity's membership event at `seq` of its chain in
    /// `pod` — its created or joined event — holds the pod.
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
    pub async fn record_pod(&self, pod: PodId, seq: Seq) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                pod_record_key(&pod, seq).into_bytes(),
                vec![1u8],
            )
            .await?;
        Ok(())
    }

    /// Record that the identity's left event at `seq`, or a removed event at
    /// `seq` its device learned of, ends its holding of `pod`: a tombstone
    /// at that sequence.
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
    pub async fn tombstone_pod(&self, pod: PodId, seq: Seq) -> Result<()> {
        self.doc
            .del(self.author, pod_record_key(&pod, seq).into_bytes())
            .await?;
        Ok(())
    }

    /// The pods the identity holds, record-level: those whose entry at
    /// the highest sequence, across all authors, is not a tombstone. Entry
    /// timestamps are never read.
    pub async fn held_pods(&self) -> Result<Vec<PodId>> {
        Ok(self
            .pod_records(PODS_PREFIX)
            .await?
            .into_iter()
            .filter(|(_pod, (_seq, held))| *held)
            .map(|(pod, _record)| pod)
            .collect())
    }

    /// The pods the identity departed, record-level: those whose entry at
    /// the highest sequence, across all authors, is a tombstone.
    pub async fn departed_pods(&self) -> Result<Vec<PodId>> {
        Ok(self
            .pod_records(PODS_PREFIX)
            .await?
            .into_iter()
            .filter(|(_pod, (_seq, held))| !*held)
            .map(|(pod, _record)| pod)
            .collect())
    }

    /// `pod`'s highest recorded sequence and whether the identity holds
    /// the pod from it; `None` for a pod never recorded.
    pub async fn pod_record(&self, pod: PodId) -> Result<Option<(Seq, bool)>> {
        let prefix = format!("{PODS_PREFIX}{pod}/");
        Ok(self
            .pod_records(&prefix)
            .await?
            .remove(&pod)
            .map(|(seq, held)| (Seq::new(seq), held)))
    }

    /// Per pod under `prefix`: its highest sequence, and whether no
    /// tombstone sits there, a tombstone outweighing a non-empty entry.
    async fn pod_records(&self, prefix: &str) -> Result<HashMap<PodId, (u64, bool)>> {
        let query = Query::all().key_prefix(prefix.as_bytes()).include_empty();
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut highest: HashMap<PodId, (u64, bool)> = HashMap::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            let Some((pod, seq)) = pod_record_of(entry.key()) else {
                continue;
            };
            let held = entry.content_len() != 0;
            let slot = highest.entry(pod).or_insert((seq, held));
            if seq > slot.0 {
                *slot = (seq, held);
            } else if seq == slot.0 {
                slot.1 &= held;
            }
        }
        Ok(highest)
    }

    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
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

    /// Written once, by the identity's creation; the payload is the 32
    /// secret bytes, from which the pair follows.
    ///
    /// Lands in the stores' write batches, on disk within their settle window;
    /// [`SyncNode::flush_stores`](crate::SyncNode::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
    pub async fn put_identity_key(&self, key: &IdentityKeyPair) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                IDENTITY_KEY_PATH.as_bytes().to_vec(),
                key.secret_bytes().to_vec(),
            )
            .await?;
        Ok(())
    }

    /// `Ok(None)` while the payload is still syncing. A payload that does
    /// not decode is an error, as for [`Self::get_ticket`].
    pub async fn identity_key(&self) -> Result<Option<IdentityKeyPair>> {
        let Some(bytes) =
            read_payload(&self.doc, &self.blobs, IDENTITY_KEY_PATH.as_bytes()).await?
        else {
            return Ok(None);
        };
        let secret: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("an identity key payload is not 32 bytes")?;
        Ok(Some(IdentityKeyPair::from_secret_bytes(&secret)))
    }

    /// Crate-private: the fork's event type stays behind this layer.
    pub(crate) async fn events(
        &self,
    ) -> Result<impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static> {
        self.doc.subscribe().await
    }

    /// A detail-free item after every observed change — an entry written
    /// here, arrived by sync, or a payload become readable; a burst past
    /// either of the subscription's two buffers arrives as one item per
    /// buffer. An `Err` item is the subscription failing; the stream ends
    /// with the node.
    pub async fn changes(&self) -> Result<impl Stream<Item = Result<()>> + Send + Unpin + 'static> {
        let events = self.events().await?;
        Ok(events.filter_map(change_of))
    }

    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
    }

    /// Taken before whatever starts the replica's sessions — before
    /// `host_identity` arms a PMS — or a session finished before the
    /// subscription goes unseen and the wait holds out for the next one.
    pub async fn watch_catch_up(&self) -> Result<CatchUpWatch> {
        watch_doc(&self.doc).await
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
    /// devices write here, so garbage is this implementation's own bug.
    pub async fn get_ticket(&self, kind: &str) -> Result<Option<DocTicket>> {
        let Some(bytes) = read_payload(&self.doc, &self.blobs, ticket_key(kind).as_bytes()).await?
        else {
            return Ok(None);
        };
        let ticket = std::str::from_utf8(&bytes)?.parse::<DocTicket>()?;
        Ok(Some(ticket))
    }
}

/// The item [`PrivateMetadataStore::changes`] yields for `event`: a lag
/// notice stands for the changes it dropped, so it reads as one.
fn change_of(event: Result<LiveEvent>) -> Option<Result<()>> {
    match event {
        Ok(
            LiveEvent::InsertLocal { .. }
            | LiveEvent::InsertRemote { .. }
            | LiveEvent::ContentReady { .. }
            | LiveEvent::Lagged,
        ) => Some(Ok(())),
        Ok(_) => None,
        Err(err) => Some(Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lag notice on the PMS's events is an item of its change stream,
    /// as an insert is; an event that changes nothing is none.
    #[test]
    fn a_lag_notice_reads_as_a_change() {
        assert!(matches!(change_of(Ok(LiveEvent::Lagged)), Some(Ok(()))));
        assert!(change_of(Ok(LiveEvent::PendingContentReady)).is_none());
    }

    /// A pending-device marker whose seconds overflow the clock reads as
    /// malformed, which the expiry sweep deletes, and one of a later marker
    /// version as unknown, which the sweep keeps.
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
