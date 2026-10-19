//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. Externally supplied protocols — pdn-node's linking
//! dialogue and a pod's join (ADR-0012, ADR-0015) — register on the same
//! endpoint at spawn; a narrow dial handle serves their dial sides. The registration
//! point is protocol-agnostic: the ceremonies' semantics live in pdn-node.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::IpAddr,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use futures_lite::{FutureExt, StreamExt};
use iroh::{
    endpoint::{default_relay_mode, presets, Connection, QuicTransportConfig, VarInt},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, Watcher as _,
};
use iroh_blobs::{
    protocol::Request,
    provider::{
        events::EventSender, handle_get, handle_get_many, handle_observe, handle_push, StreamPair,
    },
    store::{fs::FsStore, GcConfig, ProtectCb, ProtectOutcome},
    Hash, ALPN as BLOBS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use pdn_store::{
    api::{AddrInfoOptions, Doc, DocsApi},
    protocol::{Docs, DocsDispatch},
    store::Query,
    AuthorId, Contact, DocTicket, Identity, NamespaceId, ALPN as DOCS_ALPN,
};
use pdn_types::{NodeId, PdnId, PodId, RecordRef};
use rand::seq::SliceRandom as _;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use crate::{
    access::{session_access_provider, AccessBook},
    address_book::{AddressBook, ADDRESS_BOOK_INTERVAL},
    pod::{
        record_entries, record_prefix, unknown_entries, CreatedPayload, EventKind, Member,
        MemberDevice, MembershipKey, MembershipView, Operation, PodCatchUp, PodHolding, PodNotice,
        PodStore, PodTickets, RecordView, Seq, UnknownEntry, UnknownPod, Verdict,
    },
    private_metadata::PrivateMetadataStore,
    registry::{PodBinding, Registry},
};

/// The namespace was forgotten, or never imported here: a pod store's
/// contacts derived for a replica no longer tracked.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("namespace not tracked on this node: {namespace}")]
pub struct UntrackedNamespace {
    pub namespace: NamespaceId,
}

/// A protocol supplied to [`SyncNode::spawn_with`]: its ALPN and handler.
pub type ExtraProtocol = (Vec<u8>, Box<dyn DynProtocolHandler>);

/// Reserved: an extra protocol claiming one of these is refused at spawn.
pub const BUILT_IN_ALPNS: [&[u8]; 3] = [BLOBS_ALPN, GOSSIP_ALPN, DOCS_ALPN];

/// Downcast from the `anyhow::Error` of [`SyncNode::spawn_with`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("protocol ALPN already taken: {}", String::from_utf8_lossy(.alpn))]
pub struct AlpnTaken {
    pub alpn: Vec<u8>,
}

/// Another running node holds the directory. Downcast from the
/// `anyhow::Error` of the spawn entries; the underlying lock error stays in
/// the chain as the cause.
#[derive(Debug, Clone, thiserror::Error)]
#[error("storage directory {} is held by another running node", directory.display())]
pub struct DirectoryHeld {
    pub directory: std::path::PathBuf,
}

/// A panic in an extra handler's `accept` must not reach iroh's router
/// accept loop, where it is fatal to the whole node. A caught panic drops
/// that one connection; the dialer may see a clean end-of-stream rather than
/// an error. Does not survive a `panic = "abort"` build.
#[derive(Debug)]
struct PanicGuarded {
    inner: Box<dyn DynProtocolHandler>,
}

impl ProtocolHandler for PanicGuarded {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        match AssertUnwindSafe(self.inner.accept(connection))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(_panic) => Err(AcceptError::from_err(std::io::Error::other(
                "extra protocol handler panicked",
            ))),
        }
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

/// The blob protocol with its streams' tasks tracked and the store's closing
/// left to the node: the router shuts its protocols down before it cancels
/// their connections, and a stream's task outlives the router's shutdown, so
/// a request a stream already read would reach a store that is closing,
/// which iroh-blobs answers by panicking on its own task.
#[derive(Debug, Clone)]
struct BlobsServing {
    store: iroh_blobs::api::Store,
    streams: TaskTracker,
    #[cfg(feature = "test-util")]
    hold: BlobStreamHoldSlot,
}

impl ProtocolHandler for BlobsServing {
    /// `iroh_blobs::provider::handle_connection`, which spawns each stream's
    /// task untracked, without the provider events the node takes none of.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        while let Ok(pair) = StreamPair::accept(&connection, EventSender::DEFAULT).await {
            self.streams.spawn(serve_blob_stream(
                pair,
                self.store.clone(),
                #[cfg(feature = "test-util")]
                self.hold.clone(),
                #[cfg(feature = "test-util")]
                self.streams.clone(),
            ));
        }
        Ok(())
    }
}

/// `iroh_blobs::provider::handle_stream`, split at the read of the request
/// so a scenario can hold a stream between its request and the store.
async fn serve_blob_stream(
    mut pair: StreamPair,
    store: iroh_blobs::api::Store,
    #[cfg(feature = "test-util")] hold_slot: BlobStreamHoldSlot,
    #[cfg(feature = "test-util")] streams: TaskTracker,
) {
    let request = match pair.read_request().await {
        Ok(request) => request,
        Err(err) => {
            tracing::debug!("blob stream failed: {err:#}");
            return;
        }
    };
    #[cfg(feature = "test-util")]
    let held = hold_blob_stream(&hold_slot, &streams).await;
    let served = match request {
        Request::Get(request) => handle_get(pair, store, request).await,
        Request::GetMany(request) => handle_get_many(pair, store, request).await,
        Request::Observe(request) => handle_observe(pair, store, request).await,
        Request::Push(request) => handle_push(pair, store, request).await,
        _ => Ok(()),
    };
    if let Err(err) = served {
        tracing::debug!("blob stream failed: {err:#}");
    }
    #[cfg(feature = "test-util")]
    if let Some(held) = held {
        held.done.notify_one();
    }
}

/// Holds a stream that read its request, if a scenario armed a hold, until
/// the stop closes the stream tracker — its router's shutdown has returned
/// by then — or the scenario releases it.
#[cfg(feature = "test-util")]
async fn hold_blob_stream(
    hold_slot: &BlobStreamHoldSlot,
    streams: &TaskTracker,
) -> Option<Arc<BlobStreamHold>> {
    let held = hold_slot.lock().ok().and_then(|mut slot| slot.take())?;
    held.reached.notify_one();
    while !streams.is_closed() {
        tokio::select! {
            () = held.release.notified() => break,
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    Some(held)
}

/// Default of [`SpawnOptions::reconcile_interval`]. Gossip is best-effort
/// and the rescue triggers ride the same gossip, so without this pass a
/// late write can starve. Each pass re-dials a doc's import-time contacts,
/// which matter because the engine records a peer only after one successful
/// exchange — a replica whose initial exchange died would otherwise starve
/// permanently.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Default of [`SpawnOptions::pod_reconcile_interval`]: a pod store's
/// writes arrive over its swarm, and its pass catches up what gossip lost.
const POD_RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

/// The peers one run of the pod stores' pass reaches per store, so a
/// pod's sessions stay bounded whatever its size.
const POD_RECONCILE_PEERS: usize = 5;

/// The slowest link a flush waits out, in bytes a second (1 Mbit/s): a
/// payload the flushed device still lacks extends the wait by its length at
/// this rate.
const FLUSH_PAYLOAD_RATE: u64 = 125_000;

/// Payloads a flush asks a device about at once, under the 100
/// bidirectional streams a connection carries.
const FLUSH_OBSERVES: usize = 16;

/// Default of [`SpawnOptions::blob_collection_interval`].
const BLOB_COLLECTION_INTERVAL: Duration = Duration::from_secs(600);

/// How often iroh-blobs asks the collection's protect callback, a run going
/// ahead once [`SpawnOptions::blob_collection_interval`] has passed: a
/// stopped node's collection task ends at the next ask, and holds the blob
/// store's runtime until then. Each ask that skips its run logs one line at
/// info from iroh-blobs.
const BLOB_COLLECTION_TICK: Duration = Duration::from_secs(60);

/// Default of [`SpawnOptions::pod_change_settle`].
const POD_CHANGE_SETTLE: Duration = Duration::from_millis(50);

/// How long a stop waits for the blob protocol's streams and then the blob
/// store to finish: a stream ends at its next read or write on the closed
/// endpoint, and a request it left in the store at its first answer.
const BLOB_STORE_IDLE_WAIT: Duration = Duration::from_secs(5);

/// Unidirectional streams a peer holds open on one connection. Gossip keeps
/// one per swarm topic two nodes share, a pod's stores are two, and a sender
/// past the count sends that peer nothing on any topic.
const MAX_UNI_STREAMS: u32 = 4_096;

/// What one connection buffers across its streams: the 199 streams times
/// 1,250,000 bytes the transport's defaults allowed, so the larger stream
/// count gives a caller no more memory.
const CONNECTION_RECEIVE_WINDOW: u32 = 248_750_000;

/// Chosen by name at spawn, with no default. Not read from the process
/// environment: several nodes spawn in one process, and a directory belongs
/// to one node.
#[derive(Debug, Clone)]
pub enum StorageConfig {
    /// Nothing on disk and no blob collection: the workspace's suites, never
    /// a product build.
    #[cfg(any(test, feature = "test-util"))]
    Memory,
    /// `identities/<identity>/` (replica store and persisted author),
    /// `blobs/`, `node.key`, `lock`. Created owner-only when absent: the
    /// replica store holds namespace secrets and the blobs payload bytes in
    /// the clear.
    Directory(std::path::PathBuf),
}

impl StorageConfig {
    /// `None` on a node in memory.
    #[cfg_attr(
        not(any(test, feature = "test-util")),
        allow(clippy::unnecessary_wraps) // a product build has no node in memory
    )]
    fn directory(&self) -> Option<&std::path::Path> {
        match self {
            #[cfg(any(test, feature = "test-util"))]
            Self::Memory => None,
            Self::Directory(directory) => Some(directory),
        }
    }
}

/// What the endpoint binds to be reachable; each variant keeps what the one
/// before it binds. The choice is the embedding product's, since every step
/// past [`Connectivity::Direct`] routes through infrastructure the project
/// does not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// A peer is reached at an address the endpoint publishes about itself
    /// or not at all. What the suites and the container stand run on.
    Direct,
    /// Relay servers too: the relay routes by node id and its URL travels in
    /// every ticket and ceremony payload, so a peer behind a NAT is reachable
    /// and the addressing outlives the network it was minted on. Both ends'
    /// node ids are visible to whoever runs the relay.
    Relays,
    /// Address lookup as well, so a contact known by node id alone — a
    /// sibling read out of a device record — is dialable. The node id then
    /// resolves globally: anyone holding one can ask whether the device is
    /// up and which relay it is homed on, and no withdrawal takes that back.
    RelaysAndAddressLookup,
}

/// Spawn-time configuration ([`SyncNode::spawn_with`]). Build with
/// [`SpawnOptions::on_directory`] or [`SpawnOptions::for_product`]; the
/// suites also with `SpawnOptions::memory`.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Required: a spawn that names no storage is not expressible.
    pub storage: StorageConfig,
    pub reconcile_interval: Duration,
    /// The pass over each pod store, whose every run reaches at most 5
    /// peers per store; every other tracked store keeps
    /// `reconcile_interval` and every contact.
    pub pod_reconcile_interval: Duration,
    /// How long a pod's change watch gathers events that list no new
    /// device — a session's end, a neighbor — before deriving the pod's
    /// contacts again; an entry or a payload arriving derives at once.
    pub pod_change_settle: Duration,
    /// How often the node removes the payloads no replica of any identity
    /// it hosts references.
    pub blob_collection_interval: Duration,
    /// [`Connectivity::Direct`] in every constructor but
    /// [`SpawnOptions::for_product`].
    pub connectivity: Connectivity,
    /// What this node's replica stores may hold together. Divided among
    /// the identities the storage directory holds as each store opens,
    /// so no host states a count; the bound cannot be changed on an open
    /// store, so a store keeps the share it opened at until the next
    /// start.
    pub replica_cache_budget_bytes: usize,
}

/// What a node's replica stores may hold together, unless the host names
/// another: enough that a device carrying one identity never evicts what
/// a personal store holds. The only memory figure a host states, since
/// the count it is divided by is read from the storage directory.
pub const DEFAULT_REPLICA_CACHE_BUDGET_BYTES: usize = 1024 * 1024 * 1024;

impl SpawnOptions {
    /// In memory, direct paths — what the in-process suites run on.
    #[cfg(any(test, feature = "test-util"))]
    pub fn memory() -> Self {
        Self {
            storage: StorageConfig::Memory,
            reconcile_interval: RECONCILE_INTERVAL,
            pod_reconcile_interval: POD_RECONCILE_INTERVAL,
            pod_change_settle: POD_CHANGE_SETTLE,
            blob_collection_interval: BLOB_COLLECTION_INTERVAL,
            connectivity: Connectivity::Direct,
            replica_cache_budget_bytes: DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
        }
    }

    /// Under `directory`, direct paths — what the container stand runs on.
    pub fn on_directory(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            storage: StorageConfig::Directory(directory.into()),
            reconcile_interval: RECONCILE_INTERVAL,
            pod_reconcile_interval: POD_RECONCILE_INTERVAL,
            pod_change_settle: POD_CHANGE_SETTLE,
            blob_collection_interval: BLOB_COLLECTION_INTERVAL,
            connectivity: Connectivity::Direct,
            replica_cache_budget_bytes: DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
        }
    }

    /// Under `directory`, with [`Connectivity::RelaysAndAddressLookup`] —
    /// the reachability a device that moves between networks needs, kept
    /// out of [`SpawnOptions::on_directory`] so a suite whose peers already
    /// have an address for each other reaches nobody's infrastructure.
    pub fn for_product(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            connectivity: Connectivity::RelaysAndAddressLookup,
            ..Self::on_directory(directory)
        }
    }
}

/// One running node: iroh endpoint, gossip and blob store, under one
/// half per hosted identity — that identity's own docs engine, replica
/// store, registry and access book (ADR-0013). A pod's stores are
/// addressed by [`PodId`] within the identity that holds them. Every doc
/// an identity opens joins a reconcile pass: a pod's stores the pod pass,
/// every other the periodic one.
#[derive(Debug)]
pub struct SyncNode {
    router: Router,
    blobs: iroh_blobs::api::Store,
    /// The blob protocol's stream tasks, which outlive the router.
    blob_streams: TaskTracker,
    gossip: Gossip,
    /// The half of the node each hosted identity owns.
    identities: Identities,
    /// What this node's replica stores may hold together; one store's
    /// share of it is cut as that store opens.
    cache_budget_bytes: usize,
    pod_change_settle: Duration,
    /// Sessions [`reconcile_co_located`] has opened, so a scenario can
    /// assert that a pass over a quiet pair opens none.
    #[cfg(feature = "test-util")]
    co_located_sessions: CoLocatedPassSessions,
    /// Fails the record store's creation in the next `create_pod`, after
    /// its membership store exists.
    #[cfg(feature = "test-util")]
    fail_next_pod_records_create: std::sync::atomic::AtomicBool,
    /// Holds the next pod start between its two stores' starts.
    #[cfg(feature = "test-util")]
    pod_start_pause: std::sync::Mutex<Option<Arc<PodStartPause>>>,
    /// Holds the next blob collection run once it has read the replicas.
    #[cfg(feature = "test-util")]
    collection_pause: CollectionPauseSlot,
    /// Every blob collection run the protect callback was asked about.
    #[cfg(feature = "test-util")]
    collection_runs: Arc<std::sync::atomic::AtomicUsize>,
    /// Holds the next blob stream once it has read its request.
    #[cfg(feature = "test-util")]
    blob_stream_hold: BlobStreamHoldSlot,
    /// The steps of every stop, in the order they ran.
    #[cfg(feature = "test-util")]
    stop_steps: StopSteps,
    /// Handed to every hosted identity's book; empty until a scenario takes
    /// the channel, so nothing accumulates unread.
    #[cfg(feature = "test-util")]
    pod_verdicts: crate::access::PodVerdictSink,
    /// Handed to every hosted identity's book like `pod_verdicts`.
    #[cfg(feature = "test-util")]
    session_traces: crate::access::SessionTraceSink,
    /// Blob collection runs once set, at the host's
    /// [`start_blob_collection`](Self::start_blob_collection); a node in
    /// memory has none.
    collecting: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the stop once the blob store is closed: the collection's task
    /// then ends at its next ask.
    blob_store_closed: Arc<std::sync::atomic::AtomicBool>,
    /// What the passes report, so a scenario can order its absence
    /// assertions after runs that happened.
    #[cfg(feature = "test-util")]
    pass_probes: Arc<PassProbes>,
    storage: StorageConfig,
    /// Handed to every hosted identity's half; filled once, by the
    /// runtime's take.
    pod_notices: crate::pod::PodNoticeSink,
    /// Taken once, so a repeated `shutdown` is a no-op under a shared
    /// reference; both passes watch it.
    reconciler_stop: Mutex<Option<watch::Sender<()>>>,
    /// Cloned into every hosted identity's engine; the task at the other
    /// end holds the identity map weakly (`serve_co_located`).
    co_located_requests: pdn_store::engine::CoLocatedRequests,
    /// Taken by the stop with the stores, or released with the process.
    /// `None` on a memory node.
    directory_lock: Mutex<Option<std::fs::File>>,
    /// `None` on a memory node, which comes back as nobody.
    address_book: Option<Arc<AddressBook>>,
}

/// How a tracked doc re-syncs. `ContactsOnly` never joins the gossip
/// swarm — a pod's tombstone, reconciled with the contacts its membership
/// derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncStrategy {
    Swarm,
    ContactsOnly,
}

/// One doc under the reconcile pass. The engine records a peer only after
/// one successful exchange, so the import-time contacts are the only
/// recovery path for a replica whose initial exchange died. Each contact
/// carries the identity it is dialed as.
#[derive(Debug, Clone)]
struct TrackedDoc {
    doc: Doc,
    contacts: Vec<Contact>,
    /// The contacts the store's tickets named when it was tracked, which
    /// replacing `contacts` keeps.
    ticketed: Vec<Contact>,
    strategy: SyncStrategy,
    /// Whom a peer of this replica is dialed as when no contact names one:
    /// the identity for a store its devices share, the member whose ticket
    /// it was imported from for a pod's store.
    default_identity: Identity,
}

/// An identity whose subdirectory records its hosting, as a start finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHosting {
    pub identity: PdnId,
    /// The namespace of the identity's PMS.
    pub pms: NamespaceId,
    /// Whether the replica store beside the record is on disk.
    pub store_present: bool,
}

/// The hosted identities of one node, shared with the docs dispatcher and
/// the reconcile pass.
type Identities = Arc<std::sync::RwLock<HashMap<PdnId, Arc<HostedStack>>>>;

/// One hosted identity's half of the node: its own docs engine and
/// replica store, the registry its pods resolve through, the book that
/// judges its sessions, and the author its writes carry.
#[derive(Debug)]
struct HostedStack {
    identity: PdnId,
    docs: Docs,
    api: DocsApi,
    registry: Arc<Registry>,
    access: Arc<AccessBook>,
    author: AuthorId,
    /// Keyed by namespace, so a re-import replaces its entry rather than
    /// accreting a second one.
    tracked_docs: Mutex<HashMap<NamespaceId, TrackedDoc>>,
    /// Requests to reconcile with a co-located identity — a write's
    /// announcement or a contact naming this node — each held from when the
    /// node takes it up until the callee has read the session's first
    /// message or the opening failed. Another for the same pair meanwhile is
    /// dropped, not replayed to it: a later announcement or pass carries
    /// what it would have.
    announcements_in_flight: Mutex<HashSet<(NamespaceId, Identity)>>,
    /// Pods this identity departed whose tombstone has converged with a
    /// member's device, and is reconciled with the identity's own devices
    /// alone from then on.
    converged_tombstones: Mutex<HashSet<PodId>>,
    /// Pods whose membership view the last run of the pass found listing
    /// nobody.
    found_empty: Mutex<HashSet<PodId>>,
    /// One per pod, held through each derivation of that pod's contacts,
    /// from its membership view to its last write: one that read the view
    /// before a change would otherwise set its list after the one that read
    /// it after.
    pod_derivations: Mutex<HashMap<PodId, Arc<tokio::sync::Mutex<()>>>>,
    notices: crate::pod::PodNoticeSink,
}

impl HostedStack {
    /// `pod`'s derivation lock. A map of locks holds no state a panic
    /// could leave half-written, so a poisoned one is used as it stands.
    fn pod_derivation(&self, pod: PodId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .pod_derivations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(pod).or_default())
    }

    fn report(&self, notice: PodNotice) {
        if let Ok(sender) = self.notices.lock() {
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(notice);
            }
        }
    }

    fn identity(&self) -> pdn_store::Identity {
        crate::access::identity_of(self.identity)
    }

    fn track(
        &self,
        doc: &Doc,
        contacts: Vec<Contact>,
        strategy: SyncStrategy,
        default_identity: Identity,
    ) -> Result<()> {
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        docs.insert(
            doc.id(),
            TrackedDoc {
                doc: doc.clone(),
                ticketed: contacts.clone(),
                contacts,
                strategy,
                default_identity,
            },
        );
        Ok(())
    }

    /// Start the sync of a device-shared store this identity just armed,
    /// with its tracked contacts; detached, because arming is synchronous
    /// and a failed start is retried by the next pass anyway.
    fn start_armed(&self, namespace: NamespaceId) -> Result<()> {
        let Some(tracked) = self.tracked(namespace)? else {
            return Ok(());
        };
        let _detached = tokio::spawn(async move {
            let _ = tracked
                .doc
                .start_sync(tracked.contacts, tracked.default_identity)
                .await;
        });
        Ok(())
    }

    /// Replace a tracked store's contacts wholesale; an untracked store is
    /// refused rather than its set dropped silently.
    fn set_contacts(&self, namespace: NamespaceId, contacts: Vec<Contact>) -> Result<()> {
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        let entry = docs
            .get_mut(&namespace)
            .ok_or(UntrackedNamespace { namespace })?;
        entry.contacts = contacts;
        Ok(())
    }

    /// Keeps the tracked store's contacts and default identity.
    fn set_strategy(&self, namespace: NamespaceId, strategy: SyncStrategy) -> Result<()> {
        if let Some(entry) = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?
            .get_mut(&namespace)
        {
            entry.strategy = strategy;
        }
        Ok(())
    }

    fn tracked(&self, namespace: NamespaceId) -> Result<Option<TrackedDoc>> {
        Ok(self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?
            .get(&namespace)
            .cloned())
    }

    fn untrack(&self, namespace: NamespaceId) -> Result<()> {
        self.tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?
            .remove(&namespace);
        Ok(())
    }

    fn tracked_snapshot(&self) -> Vec<TrackedDoc> {
        match self.tracked_docs.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(_poisoned) => Vec::new(),
        }
    }
}

/// A node dropped without [`shutdown`](SyncNode::shutdown) stops as a
/// stopped one does, in a task of its own; dropped outside a tokio runtime,
/// it stops nothing but its directory lock.
impl Drop for SyncNode {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Some(stop) = self
            .reconciler_stop
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        {
            let _ = stop.send(());
        }
        let identities = Arc::clone(&self.identities);
        let router = self.router.clone();
        let blob_streams = self.blob_streams.clone();
        let blobs = self.blobs.clone();
        let blob_store_closed = Arc::clone(&self.blob_store_closed);
        let directory_lock = self.directory_lock.get_mut().ok().and_then(Option::take);
        #[cfg(feature = "test-util")]
        let stop_steps = Arc::clone(&self.stop_steps);
        let _detached = runtime.spawn(async move {
            let _ = stop_node(
                &identities,
                &router,
                &blob_streams,
                &blobs,
                &blob_store_closed,
                directory_lock,
                #[cfg(feature = "test-util")]
                &stop_steps,
            )
            .await;
        });
    }
}

/// The stop [`SyncNode::shutdown`] and a node's drop share: every engine,
/// then the router, then the blob protocol's streams, then the blob store
/// and the directory lock (the `node-assembly` spec, "Shutdown stops every
/// protocol side by side, and a late call fails").
async fn stop_node(
    identities: &Identities,
    router: &Router,
    blob_streams: &TaskTracker,
    blobs: &iroh_blobs::api::Store,
    blob_store_closed: &std::sync::atomic::AtomicBool,
    directory_lock: Option<std::fs::File>,
    #[cfg(feature = "test-util")] stop_steps: &StopSteps,
) -> Result<()> {
    #[cfg(feature = "test-util")]
    let step = |done: StopStep| {
        if let Ok(mut steps) = stop_steps.lock() {
            steps.push(done);
        }
    };
    // Before the blob store closes: a running engine starts downloads that
    // read it. The router serves the dispatcher, not the engines, so each
    // engine is shut down by name.
    let stacks: Vec<Arc<HostedStack>> = match identities.read() {
        Ok(hosted) => hosted.values().cloned().collect(),
        Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
    };
    for stack in stacks {
        let _ = stack.docs.engine().shutdown().await;
    }
    #[cfg(feature = "test-util")]
    step(StopStep::Engines);
    // Everything below runs whatever the router answers: a stop that
    // returned early would leave the blob store and the directory lock
    // held for the rest of the process.
    let routing_stopped = router.shutdown().await;
    #[cfg(feature = "test-util")]
    step(StopStep::Router);
    // A request a stream read before the router's shutdown still reaches
    // the store, and the store's shutdown closes its database beneath it
    // at once: the read then poisons the entry and iroh-blobs panics on
    // its own task.
    blob_streams.close();
    let finished = tokio::time::timeout(BLOB_STORE_IDLE_WAIT, async {
        blob_streams.wait().await;
        blobs.wait_idle().await
    })
    .await;
    if finished.is_err() {
        tracing::warn!("the blob store was still busy when the node stopped");
    }
    #[cfg(feature = "test-util")]
    step(StopStep::BlobStreams);
    // Explicit rather than on the last handle's drop: a node respawned on
    // the same directory would meet its predecessor's database lock.
    // Best-effort: a store already shut down answers with an error.
    let _ = blobs.shutdown().await;
    blob_store_closed.store(true, std::sync::atomic::Ordering::Release);
    #[cfg(feature = "test-util")]
    step(StopStep::BlobStore);
    // With the stores, not with the node's drop: a detached task holding
    // the node alive a moment longer must not make a spawn on the same
    // directory read as a second running node.
    if let Some(lock) = directory_lock {
        let _ = lock.unlock();
    }
    routing_stopped?;
    Ok(())
}

/// The dial side of a node's protocols: connect out, read the node's own
/// address and wire id — never the endpoint's lifecycle, which stays the
/// node's own.
#[derive(Debug, Clone)]
pub struct DialHandle {
    endpoint: Endpoint,
}

impl DialHandle {
    /// The peer must serve `alpn` or the dial fails.
    pub async fn connect(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
        Ok(self.endpoint.connect(addr, alpn).await?)
    }

    /// This node's wire id plus the paths peers can reach it on.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }
}

impl SyncNode {
    pub async fn spawn(options: SpawnOptions) -> Result<Self> {
        Self::spawn_with(Vec::new(), options).await
    }

    /// Extra protocols are served on the same endpoint next to the built-in
    /// ones (ADR-0012, ADR-0015), each dispatched as a raw bidirectional
    /// connection. An ALPN collision fails the spawn with [`AlpnTaken`]
    /// before anything binds. A handler's `accept` should return
    /// `Err(AcceptError)` rather than panic: a panic is contained per
    /// connection, but a `panic = "abort"` build still aborts the process.
    pub async fn spawn_with(
        extra_protocols: Vec<ExtraProtocol>,
        options: SpawnOptions,
    ) -> Result<Self> {
        Self::assemble(extra_protocols, options, true).await
    }

    /// A node that serves no payload: a peer's fetch of its entries'
    /// payloads fails at the dial and is tried again, so a download stays
    /// under way on the peer for as long as a scenario needs.
    #[cfg(feature = "test-util")]
    pub async fn spawn_serving_no_payloads_for_test(options: SpawnOptions) -> Result<Self> {
        Self::assemble(Vec::new(), options, false).await
    }

    #[allow(clippy::too_many_lines)] // one node assembled in order, each part beside what it feeds
    async fn assemble(
        extra_protocols: Vec<ExtraProtocol>,
        options: SpawnOptions,
        serve_payloads: bool,
    ) -> Result<Self> {
        // An extra silently replacing a built-in handler would leave a node
        // that looks alive and never syncs.
        let mut taken: HashSet<&[u8]> = BUILT_IN_ALPNS.into_iter().collect();
        for (alpn, _handler) in &extra_protocols {
            if !taken.insert(alpn.as_slice()) {
                return Err(AlpnTaken { alpn: alpn.clone() }.into());
            }
        }

        let (secret_key, directory_lock, address_book) = prepare_storage(&options.storage).await?;

        let endpoint = bind_endpoint(
            secret_key,
            options.connectivity,
            address_book.as_ref().map(|book| book.lookup()),
        )
        .await?;
        let identities: Identities = Arc::default();
        let collecting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let blob_store_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(feature = "test-util")]
        let collection_pause = CollectionPauseSlot::default();
        #[cfg(feature = "test-util")]
        let collection_runs = Arc::<std::sync::atomic::AtomicUsize>::default();
        let blobs_store = open_blob_store(
            &options,
            &identities,
            &collecting,
            &blob_store_closed,
            #[cfg(feature = "test-util")]
            &collection_pause,
            #[cfg(feature = "test-util")]
            &collection_runs,
        )
        .await?;
        let gossip = Gossip::builder().spawn(endpoint.clone());

        let resolver: pdn_store::protocol::IdentityResolver = {
            let identities = Arc::clone(&identities);
            Arc::new(move |identity: pdn_store::Identity| {
                let hosted = identities.read().ok()?;
                hosted
                    .values()
                    .find(|stack| stack.identity() == identity)
                    .map(|stack| stack.docs.clone())
            })
        };

        let blob_streams = TaskTracker::new();
        #[cfg(feature = "test-util")]
        let blob_stream_hold = BlobStreamHoldSlot::default();
        let mut router = Router::builder(endpoint);
        if serve_payloads {
            router = router.accept(
                BLOBS_ALPN,
                BlobsServing {
                    store: blobs_store.clone(),
                    streams: blob_streams.clone(),
                    #[cfg(feature = "test-util")]
                    hold: Arc::clone(&blob_stream_hold),
                },
            );
        }
        router = router
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, DocsDispatch::new(resolver));
        for (alpn, handler) in extra_protocols {
            router = router.accept(alpn, PanicGuarded { inner: handler });
        }
        let router = router.spawn();

        let (co_located_requests, requests) =
            tokio::sync::mpsc::channel(CO_LOCATED_REQUESTS_CAPACITY);
        let _detached = tokio::spawn(serve_co_located(Arc::downgrade(&identities), requests));
        let (reconciler_stop, stop) = watch::channel(());
        let co_located_sessions: CoLocatedPassSessions = Arc::default();
        let pass_probes: Arc<PassProbes> = Arc::default();
        let _detached = tokio::spawn(reconcile_pass(
            options.reconcile_interval,
            Arc::clone(&identities),
            Arc::clone(&co_located_sessions),
            Arc::clone(&pass_probes),
            stop.clone(),
        ));
        if let Some(book) = &address_book {
            let _detached = tokio::spawn(keep_address_book(
                Arc::clone(book),
                router.endpoint().clone(),
                Arc::clone(&identities),
                stop.clone(),
            ));
        }
        let _detached = tokio::spawn(pod_reconcile_pass(
            options.pod_reconcile_interval,
            router.endpoint().id(),
            Arc::clone(&identities),
            Arc::clone(&pass_probes),
            stop,
        ));
        Ok(Self {
            router,
            blobs: blobs_store,
            blob_streams,
            gossip,
            identities,
            cache_budget_bytes: options.replica_cache_budget_bytes,
            pod_change_settle: options.pod_change_settle,
            #[cfg(feature = "test-util")]
            co_located_sessions: Arc::clone(&co_located_sessions),
            #[cfg(feature = "test-util")]
            fail_next_pod_records_create: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "test-util")]
            pod_start_pause: std::sync::Mutex::default(),
            #[cfg(feature = "test-util")]
            collection_pause,
            #[cfg(feature = "test-util")]
            collection_runs,
            #[cfg(feature = "test-util")]
            blob_stream_hold,
            #[cfg(feature = "test-util")]
            stop_steps: StopSteps::default(),
            #[cfg(feature = "test-util")]
            pod_verdicts: Arc::default(),
            #[cfg(feature = "test-util")]
            session_traces: Arc::default(),
            #[cfg(feature = "test-util")]
            pass_probes,
            collecting,
            blob_store_closed,
            storage: options.storage,
            pod_notices: Arc::default(),
            reconciler_stop: Mutex::new(Some(reconciler_stop)),
            co_located_requests,
            directory_lock: Mutex::new(directory_lock),
            address_book,
        })
    }

    /// Bring up the half of the node that hosts `identity`: its own docs
    /// engine and replica store, its registry and the book that judges
    /// its sessions (ADR-0013). The store opens under the identity's own
    /// subdirectory, bounded at its share of the node's cache budget, cut
    /// as the store opens. Provisioning an identity hosted before the call
    /// starts is a no-op; two calls for one identity at once are not
    /// supported — the check and the insert are two awaits apart — and the
    /// caller serializes them.
    pub async fn provision_identity(&self, identity: PdnId) -> Result<()> {
        if self.stack(identity)?.is_some() {
            return Ok(());
        }
        let registry = Arc::new(Registry::default());
        let access = Arc::new(AccessBook::new(identity));
        access.set_blobs(self.blobs.clone());
        #[cfg(feature = "test-util")]
        access.set_pod_verdicts(Arc::clone(&self.pod_verdicts));
        #[cfg(feature = "test-util")]
        access.set_session_traces(Arc::clone(&self.session_traces));
        let docs = self
            .docs_builder(identity, &access, &registry)?
            .co_located_requests(self.co_located_requests.clone())
            .spawn(
                self.router.endpoint().clone(),
                self.blobs.clone(),
                self.gossip.clone(),
            )
            .await
            .map_err(|err| annotate_store_error(err, &self.storage))?;
        access.set_sync(docs.engine().sync.clone());
        let api = docs.api().clone();
        let author = api.author_default().await?;
        let stack = Arc::new(HostedStack {
            identity,
            docs,
            api,
            registry,
            access,
            author,
            tracked_docs: Mutex::new(HashMap::new()),
            announcements_in_flight: Mutex::new(HashSet::new()),
            converged_tombstones: Mutex::new(HashSet::new()),
            found_empty: Mutex::new(HashSet::new()),
            pod_derivations: Mutex::new(HashMap::new()),
            notices: Arc::clone(&self.pod_notices),
        });
        let mut hosted = self
            .identities
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?;
        hosted.insert(identity, stack);
        Ok(())
    }

    /// Sessions the periodic pass over the co-located pairs has opened for
    /// `namespace` — what shows that a pass over a quiet pair opens none.
    #[cfg(feature = "test-util")]
    pub fn co_located_pass_sessions_of(&self, namespace: NamespaceId) -> u64 {
        self.co_located_sessions
            .lock()
            .ok()
            .and_then(|opened| opened.get(&namespace).copied())
            .unwrap_or_default()
    }

    /// The store one hosted identity opens: in memory, or under that
    /// identity's own subdirectory, bounded at the node's budget divided
    /// by the identities that directory holds.
    fn docs_builder(
        &self,
        identity: PdnId,
        access: &Arc<AccessBook>,
        registry: &Arc<Registry>,
    ) -> Result<pdn_store::protocol::Builder> {
        // The store's own name for the identity: the same one, as opaque
        // bytes it compares and never interprets.
        let held_for = crate::access::identity_of(identity);
        let provider = session_access_provider(Arc::clone(access), Arc::clone(registry));
        Ok(match &self.storage {
            #[cfg(any(test, feature = "test-util"))]
            StorageConfig::Memory => Docs::memory(held_for, provider),
            StorageConfig::Directory(directory) => {
                let own = identity_directory(directory, identity);
                std::fs::create_dir_all(&own).with_context(|| {
                    format!("cannot create the identity directory {}", own.display())
                })?;
                // Cut from the identities the directory records as hosted,
                // this one included: the nth opens at an nth of the budget,
                // and what an unfinished create left takes no share.
                let held = hosted_identity_count(directory, identity)?;
                let share = self.cache_budget_bytes / held;
                tracing::info!(
                    identity = %identity,
                    share_bytes = share,
                    identities = held,
                    "the replica store opens at its share of the node's cache budget"
                );
                Docs::persistent(own, share, held_for, provider)
            }
        })
    }

    /// What the caches of this node's replica stores may together hold:
    /// the bounds handed out. An identity provisioned while the node runs
    /// leaves the stores already open at shares cut from a smaller set, so
    /// the sum passes the budget until the next start cuts every share from
    /// the whole set.
    pub fn replica_cache_ceilings_bytes(&self) -> Result<usize> {
        Ok(self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?
            .values()
            .filter_map(|stack| stack.docs.replica_cache_bytes())
            .sum())
    }

    /// Whether the bounds handed out together pass the node's budget.
    pub fn replica_cache_budget_exceeded(&self) -> Result<bool> {
        Ok(self.replica_cache_ceilings_bytes()? > self.cache_budget_bytes)
    }

    /// The bound `identity`'s replica store opened at, as the store reports
    /// it; `None` on a node storing in memory, where a store carries no
    /// bound.
    pub fn replica_cache_share_bytes(&self, identity: PdnId) -> Result<Option<usize>> {
        Ok(self.require(identity)?.docs.replica_cache_bytes())
    }

    /// Record `identity` as hosted here, with `pms` as its private
    /// PMS: the commit point of a create or a link. The identity's stores
    /// are flushed first, so the record never names a replica, or an
    /// identity key, the disk does not hold, and a failed commit fails the
    /// call; the record is written beside, synced and renamed over, so a
    /// process that dies mid-write leaves none. The
    /// parent directory is not synced after the rename, so an OS crash or a
    /// power loss can take the record back after this returned `Ok`. A node
    /// in memory records nothing.
    pub async fn record_hosting(&self, identity: PdnId, pms: NamespaceId) -> Result<()> {
        let Some(root) = self.storage.directory() else {
            return Ok(());
        };
        self.flush_stores(identity).await?;
        let own = identity_directory(root, identity);
        tokio::task::spawn_blocking(move || write_hosting_record(&own, pms))
            .await
            .context("the hosting record writer did not run")?
    }

    /// Every identity whose subdirectory records its hosting. A
    /// subdirectory without a record — an unfinished create or link — is
    /// not listed, and an unreadable record fails, naming its file. Empty on
    /// a node in memory.
    pub fn recorded_hosting(&self) -> Result<Vec<RecordedHosting>> {
        let Some(root) = self.storage.directory() else {
            return Ok(Vec::new());
        };
        read_hosting_records(root)
    }

    /// Arm `identity`'s PMS for session classification: its device
    /// records decide who is one of its devices, and its pods' stores
    /// serve fail-closed from here on. The PMS's sync starts here,
    /// after the arming, so its first session is one the book can judge.
    pub fn host_identity(&self, identity: PdnId, pms: &PrivateMetadataStore) -> Result<()> {
        let stack = self.require(identity)?;
        stack.access.arm_pms(pms.doc_handle())?;
        stack.start_armed(pms.namespace())?;
        watch_own_devices(&stack, self.router.endpoint().id(), pms.doc_handle());
        Ok(())
    }

    /// The rollback counterpart of [`host_identity`](Self::host_identity):
    /// drop everything provisioned for `identity` — its engine, its
    /// replica store handles and its registrations. What it left on disk
    /// stays there; a start hosts nothing a caller does not name.
    pub async fn unhost_identity(&self, identity: PdnId) -> Result<()> {
        let stack = {
            let mut hosted = self
                .identities
                .write()
                .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?;
            hosted.remove(&identity)
        };
        let Some(stack) = stack else {
            return Ok(());
        };
        stack.access.disarm_pms()?;
        stack.docs.engine().shutdown().await?;
        Ok(())
    }

    /// The docs under the reconcile pass — the only anchor a scenario has
    /// for a cancelled attempt whose replica has no other name.
    #[cfg(feature = "test-util")]
    pub fn tracked_doc_count(&self, identity: PdnId) -> Result<usize> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(0);
        };
        Ok(stack.tracked_snapshot().len())
    }

    /// The replicas `identity`'s store holds, tracked or not — the only
    /// anchor a scenario has for a replica no ticket ever named.
    #[cfg(feature = "test-util")]
    pub async fn held_replica_count(&self, identity: PdnId) -> Result<usize> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(0);
        };
        let mut listed = stack.api.list().await?;
        let mut held = 0;
        while let Some(entry) = listed.next().await {
            let _replica = entry?;
            held += 1;
        }
        Ok(held)
    }

    /// Take `identity`'s replica of `namespace` out of its gossip swarm,
    /// its reconciliation left running: no announcement reaches it. It holds
    /// until this node's next reconcile pass, which re-joins the swarm and
    /// opens sessions to the contacts itself.
    #[cfg(feature = "test-util")]
    pub async fn leave_swarm_for_test(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
    ) -> Result<()> {
        let tracked = self
            .require(identity)?
            .tracked(namespace)?
            .context("the identity tracks no replica of that namespace")?;
        tracked.doc.leave_gossip().await
    }

    /// Open one session for `identity`'s replica of `namespace` against
    /// `contact`, naming `caller` as the identity this side acts for —
    /// whoever this node hosts: the session a caller that claims an
    /// identity it does not hold produces. No session access is asked on
    /// the dialing side, so it serves its replica whole. `Err` is the
    /// peer's refusal.
    #[cfg(feature = "test-util")]
    pub async fn sync_namespace_as_for_test(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        contact: Contact,
        caller: Identity,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        // A session for a pair the engines are already reconciling is
        // refused for that alone, whatever the records say, so the
        // verdict asked for here is the next one. The budget covers a few
        // reconcile intervals of a scenario's cadence.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let outcome = pdn_store::net::connect_and_sync(
                self.router.endpoint(),
                &stack.docs.engine().sync,
                namespace,
                contact.identity,
                caller,
                contact.addr.clone(),
                None,
                pdn_store::admit_all(),
            )
            .await;
            match outcome {
                Ok(_finished) => return Ok(()),
                Err(pdn_store::net::ConnectError::RemoteAbort(
                    pdn_store::net::AbortReason::AlreadySyncing,
                )) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Sessions this identity opened over the in-process path, whatever
    /// asked for them: a co-located dial, a write announcement, or a
    /// contact that resolves to this node.
    #[cfg(feature = "test-util")]
    pub fn in_process_sessions(&self, identity: PdnId) -> Result<u64> {
        Ok(self
            .stack(identity)?
            .map(|stack| stack.docs.engine().in_process_sessions())
            .unwrap_or_default())
    }

    /// Sessions `identity` was handed over the in-process path by `caller`:
    /// what shows a dial reached only the identity it named.
    #[cfg(feature = "test-util")]
    pub fn in_process_sessions_served(&self, identity: PdnId, caller: PdnId) -> Result<u64> {
        Ok(self
            .stack(identity)?
            .map(|stack| {
                stack
                    .docs
                    .engine()
                    .in_process_sessions_served(crate::access::identity_of(caller))
            })
            .unwrap_or_default())
    }

    /// Refuses a ticket naming a namespace this identity already holds in
    /// another role: a store of one of its pods, or its PMS. A
    /// device-shared store is classified on its ticket alone (Invariant 1),
    /// so a pod's store that took that role by a ticket's word would be
    /// served whole, past the membership that bounds it. The mirror of
    /// `open_doc`'s guard, on the path a ticket takes.
    fn guard_shared_import(stack: &HostedStack, namespace: NamespaceId) -> Result<()> {
        let role = if stack.registry.pod_of(namespace)?.is_some() {
            "a store of a pod this identity holds"
        } else if let Some(role) = stack.access.ticket_bound_role(namespace)? {
            role
        } else {
            return Ok(());
        };
        Err(anyhow::anyhow!(
            "namespace {namespace} is {role}; a device-shared import must not repurpose it"
        ))
    }

    /// Create both stores of `pod` for `identity`, both or neither: they
    /// join the registry and the reconcile pass together, once both exist,
    /// and their sync starts once they are registered, so their first
    /// sessions are ones the book can judge. A failure leaves neither; a call
    /// dropped midway leaves what it made, so its caller runs it to its end.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn create_pod(&self, identity: PdnId, pod: PodId) -> Result<()> {
        let stack = self.require(identity)?;
        if stack.registry.pod(pod)?.is_some() {
            return Err(anyhow::anyhow!(
                "identity {identity} already holds pod {pod}"
            ));
        }
        let membership = stack.api.create().await?;
        let records = match self.create_pod_records(&stack).await {
            Ok(records) => records,
            Err(err) => {
                // Fresh and never shared: dropping it loses nothing.
                if let Err(drop_err) = stack.api.drop_doc(membership.id()).await {
                    tracing::warn!(%pod, "a membership store whose pod failed to create stayed in the store: {drop_err:#}");
                }
                return Err(err);
            }
        };
        let registered = stack.registry.register_pod(
            pod,
            PodBinding {
                membership: membership.clone(),
                records: Some(records.clone()),
            },
        );
        if let Err(err) = registered {
            for doc in [&membership, &records] {
                if let Err(drop_err) = stack.api.drop_doc(doc.id()).await {
                    tracing::warn!(%pod, "a store whose pod failed to register stayed in the store: {drop_err:#}");
                }
            }
            return Err(err);
        }
        let started: Result<()> = async {
            stack.track(
                &membership,
                Vec::new(),
                SyncStrategy::Swarm,
                stack.identity(),
            )?;
            stack.track(&records, Vec::new(), SyncStrategy::Swarm, stack.identity())?;
            Self::order_pod_stores(&stack, &membership, &records).await?;
            self.watch_pod(&stack, pod, &membership);
            start_pod_store(&membership, Vec::new(), stack.identity(), true).await?;
            start_pod_store(&records, Vec::new(), stack.identity(), true).await
        }
        .await;
        if let Err(err) = started {
            if let Err(discard_err) = self.discard_pod(identity, pod).await {
                tracing::warn!(%pod, "a pod whose sync failed to start stayed held: {discard_err:#}");
            }
            return Err(err);
        }
        Ok(())
    }

    /// Every dial of the record store follows an exchange of the membership
    /// store with the same counterpart, so the session that serves it judges
    /// by the membership view that exchange brought.
    async fn order_pod_stores(stack: &HostedStack, membership: &Doc, records: &Doc) -> Result<()> {
        stack
            .docs
            .engine()
            .order_after(records.id(), Some(membership.id()))
            .await
    }

    async fn create_pod_records(&self, stack: &HostedStack) -> Result<Doc> {
        #[cfg(feature = "test-util")]
        if self
            .fail_next_pod_records_create
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(anyhow::anyhow!("record store creation failed for test"));
        }
        stack.api.create().await
    }

    #[cfg(feature = "test-util")]
    pub fn fail_next_pod_records_create_for_test(&self) {
        self.fail_next_pod_records_create
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Holds the next pod start once its membership store's sync has
    /// started, before its record store's, until `release` is notified.
    #[cfg(feature = "test-util")]
    pub fn pause_next_pod_start_for_test(&self) -> Arc<PodStartPause> {
        let pause = Arc::new(PodStartPause::default());
        if let Ok(mut slot) = self.pod_start_pause.lock() {
            *slot = Some(Arc::clone(&pause));
        }
        pause
    }

    /// Holds `identity`'s next dial on `namespace` to `peer` once its access
    /// is decided, before the decision returns to the engine, until
    /// `release` is notified.
    #[cfg(feature = "test-util")]
    pub fn pause_next_dial_for_test(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        peer: NodeId,
    ) -> Result<Arc<crate::access::SessionPause>> {
        Ok(self.require(identity)?.access.pause_next_session(
            namespace,
            peer,
            pdn_store::SessionRole::Dial,
        ))
    }

    /// Holds the next session `identity` accepts on `namespace` from `peer`
    /// once its access is decided, before the decision returns to the
    /// engine, until `release` is notified.
    #[cfg(feature = "test-util")]
    pub fn pause_next_accept_for_test(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        peer: NodeId,
    ) -> Result<Arc<crate::access::SessionPause>> {
        Ok(self.require(identity)?.access.pause_next_session(
            namespace,
            peer,
            pdn_store::SessionRole::Accept,
        ))
    }

    /// Holds the next blob collection run once every hosted identity's
    /// replicas are read, before it removes anything, until `release` is
    /// notified.
    #[cfg(feature = "test-util")]
    pub fn pause_next_collection_for_test(&self) -> Arc<CollectionPause> {
        let pause = Arc::new(CollectionPause::default());
        if let Ok(mut slot) = self.collection_pause.lock() {
            *slot = Some(Arc::clone(&pause));
        }
        pause
    }

    /// Holds the next blob stream once it has read its request, before it
    /// reaches the blob store, until `release` is notified or the node's
    /// stop has shut its router down.
    #[cfg(feature = "test-util")]
    pub fn hold_next_blob_stream_for_test(&self) -> Arc<BlobStreamHold> {
        let hold = Arc::new(BlobStreamHold::default());
        if let Ok(mut slot) = self.blob_stream_hold.lock() {
            *slot = Some(Arc::clone(&hold));
        }
        hold
    }

    /// The steps every stop of this node took, a drop's included, in the
    /// order they ran.
    #[cfg(feature = "test-util")]
    pub fn stop_steps_for_test(&self) -> StopSteps {
        Arc::clone(&self.stop_steps)
    }

    /// The blob collection runs this node's protect callback was asked
    /// about since spawn; a count that grows after the node stopped is a
    /// collection task outliving its store.
    #[cfg(feature = "test-util")]
    pub fn collection_runs_for_test(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.collection_runs)
    }

    /// Import both stores of `pod` from their write tickets: nothing new for
    /// a pod held on these stores, the pod again for its tombstone. Both
    /// stores' sync starts once they are registered, as at `create_pod`. A failed import drops nothing — a replica may predate
    /// it — and a retry lands on it.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn import_pod(
        &self,
        identity: PdnId,
        pod: PodId,
        tickets: PodTickets,
    ) -> Result<PodCatchUp> {
        self.import_pod_with(identity, pod, tickets, &[]).await
    }

    /// [`import_pod`](Self::import_pod) with the nodes of `others` —
    /// tickets to the same two stores that other identities minted — among
    /// the contacts as well, each dialed as the identity whose ticket names
    /// it: a ticket names all its nodes as the one identity that minted it.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn import_pod_with(
        &self,
        identity: PdnId,
        pod: PodId,
        tickets: PodTickets,
        others: &[PodTickets],
    ) -> Result<PodCatchUp> {
        let stack = self.require(identity)?;
        let held = Self::admit_pod_tickets(&stack, pod, &tickets)?;
        let membership_contacts = contacts_of(
            &tickets.membership,
            others.iter().map(|other| &other.membership),
        )?;
        let records_contacts =
            contacts_of(&tickets.records, others.iter().map(|other| &other.records))?;
        if let Some(held) = held {
            return match held.records {
                Some(records) => {
                    self.start_pod(&stack, pod, &held.membership, &records)
                        .await
                }
                None => {
                    self.import_records_onto_tombstone(
                        &stack,
                        pod,
                        &held.membership,
                        tickets.records,
                        records_contacts,
                    )
                    .await
                }
            };
        }
        let (membership_minted_by, records_minted_by) =
            (tickets.membership.identity, tickets.records.identity);
        let membership = stack
            .api
            .import_namespace(tickets.membership.capability)
            .await?;
        let records = stack
            .api
            .import_namespace(tickets.records.capability)
            .await?;
        stack.registry.register_pod(
            pod,
            PodBinding {
                membership: membership.clone(),
                records: Some(records.clone()),
            },
        )?;
        stack.track(
            &membership,
            membership_contacts,
            SyncStrategy::Swarm,
            membership_minted_by,
        )?;
        stack.track(
            &records,
            records_contacts,
            SyncStrategy::Swarm,
            records_minted_by,
        )?;
        self.watch_pod(&stack, pod, &membership);
        self.start_pod(&stack, pod, &membership, &records).await
    }

    /// A join after a departure: the record store imported again beside the
    /// tombstone, which goes back into its swarm and kept its watch.
    async fn import_records_onto_tombstone(
        &self,
        stack: &HostedStack,
        pod: PodId,
        membership: &Doc,
        ticket: DocTicket,
        contacts: Vec<Contact>,
    ) -> Result<PodCatchUp> {
        Self::guard_pod_import(stack, ticket.capability.id())?;
        let minted_by = ticket.identity;
        let records = stack.api.import_namespace(ticket.capability).await?;
        stack.registry.set_pod_records(pod, Some(records.clone()))?;
        stack.track(&records, contacts, SyncStrategy::Swarm, minted_by)?;
        stack.set_strategy(membership.id(), SyncStrategy::Swarm)?;
        self.start_pod(stack, pod, membership, &records).await
    }

    fn watch_pod(&self, stack: &Arc<HostedStack>, pod: PodId, membership: &Doc) {
        let node = self.router.endpoint().id();
        watch_pod_membership(stack, node, pod, membership, self.pod_change_settle);
    }

    /// Start both tracked stores' sync, the record store's every dial
    /// ordered after the membership store's, behind a wait for the first
    /// session of each that this start brings.
    async fn start_pod(
        &self,
        stack: &HostedStack,
        pod: PodId,
        membership: &Doc,
        records: &Doc,
    ) -> Result<PodCatchUp> {
        let held = PodBinding {
            membership: membership.clone(),
            records: Some(records.clone()),
        };
        derive_before_start(stack, self.router.endpoint().id(), pod, &held).await?;
        if let Ok(missing @ 1..) =
            entries_without_payload(membership, stack.author, &self.blobs).await
        {
            tracing::warn!(
                identity = %stack.identity,
                %pod,
                missing,
                "membership entries this device wrote have no payload on disk: a kill cut them within the stores' settle window"
            );
        }
        let caught_up = PodCatchUp {
            membership: crate::private_metadata::watch_doc(membership).await?,
            records: crate::private_metadata::watch_doc(records).await?,
        };
        Self::order_pod_stores(stack, membership, records).await?;
        // Read before either starts: the membership store's first session
        // derives both stores' contacts again, from a membership view that
        // can list no device of the inviter yet, and the record store would
        // start with none.
        let (membership, records) = (
            stack.tracked(membership.id())?,
            stack.tracked(records.id())?,
        );
        Self::start_tracked(membership).await?;
        #[cfg(feature = "test-util")]
        {
            let pause = self
                .pod_start_pause
                .lock()
                .ok()
                .and_then(|mut slot| slot.take());
            if let Some(pause) = pause {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
        Self::start_tracked(records).await?;
        Ok(caught_up)
    }

    /// Drop both of `pod`'s stores and its registration, as if the pod
    /// had never been held here: the undo of a create or a join before
    /// anything of the pod left this device. Nothing for a pod not held.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn discard_pod(&self, identity: PdnId, pod: PodId) -> Result<()> {
        let stack = self.require(identity)?;
        let Some(held) = stack.registry.unregister_pod(pod)? else {
            return Ok(());
        };
        for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
            self.forget_doc(identity, doc.id()).await?;
        }
        stack
            .pod_derivations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&pod);
        Ok(())
    }

    /// Start a tracked pod store's sync with the contacts and the default
    /// identity it is tracked with.
    async fn start_tracked(tracked: Option<TrackedDoc>) -> Result<()> {
        let Some(tracked) = tracked else {
            return Ok(());
        };
        start_pod_store(
            &tracked.doc,
            tracked.contacts,
            tracked.default_identity,
            true,
        )
        .await
    }

    /// `Ok` when [`import_pod`](Self::import_pod) would take `tickets` as
    /// `pod`'s stores for `identity`, checked with nothing imported; the
    /// error is the refusal the import would answer.
    pub fn check_pod_import(
        &self,
        identity: PdnId,
        pod: PodId,
        tickets: &PodTickets,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        Self::admit_pod_tickets(&stack, pod, tickets).map(|_held| ())
    }

    /// The binding `pod` is held on, when `tickets` name its stores; `None`
    /// for a pod not held whose stores no other role holds.
    fn admit_pod_tickets(
        stack: &HostedStack,
        pod: PodId,
        tickets: &PodTickets,
    ) -> Result<Option<PodBinding>> {
        let (membership_namespace, records_namespace) = (
            tickets.membership.capability.id(),
            tickets.records.capability.id(),
        );
        if membership_namespace == records_namespace {
            return Err(anyhow::anyhow!(
                "namespace {membership_namespace} cannot be both stores of pod {pod}"
            ));
        }
        let Some(held) = stack.registry.pod(pod)? else {
            Self::guard_pod_import(stack, membership_namespace)?;
            Self::guard_pod_import(stack, records_namespace)?;
            return Ok(None);
        };
        if held.membership.id() != membership_namespace {
            return Err(anyhow::anyhow!(
                "pod {pod} is held on membership store {}, not {membership_namespace}",
                held.membership.id()
            ));
        }
        match &held.records {
            Some(records) if records.id() != records_namespace => {
                return Err(anyhow::anyhow!(
                    "pod {pod} is held on record store {}, not {records_namespace}",
                    records.id()
                ));
            }
            Some(_records) => {}
            None => Self::guard_pod_import(stack, records_namespace)?,
        }
        Ok(Some(held))
    }

    /// Refuses a ticket naming a namespace this identity already holds in
    /// any role: whatever bounded that replica's sessions before would stop
    /// bounding them once a ticket's word made it a pod's store.
    fn guard_pod_import(stack: &HostedStack, namespace: NamespaceId) -> Result<()> {
        let role = if let Some((other, _store)) = stack.registry.pod_of(namespace)? {
            format!("a store of pod {other}")
        } else if let Some(role) = stack.access.ticket_bound_role(namespace)? {
            role.to_owned()
        } else if stack.tracked(namespace)?.is_some() {
            "a store this identity's devices share".to_owned()
        } else {
            return Ok(());
        };
        Err(anyhow::anyhow!(
            "namespace {namespace} is {role}; a pod import must not repurpose it"
        ))
    }

    /// Open `pod`'s tombstone from its membership store's write ticket, as
    /// a departure leaves it: no record store, and the membership store out
    /// of its swarm, reconciled with the contacts its membership derives.
    /// Nothing for a pod `identity` holds already, a tombstone included.
    pub async fn open_pod_tombstone(
        &self,
        identity: PdnId,
        pod: PodId,
        membership: DocTicket,
        others: &[DocTicket],
    ) -> Result<()> {
        let stack = self.require(identity)?;
        if stack.registry.pod(pod)?.is_some() {
            return Ok(());
        }
        Self::guard_pod_import(&stack, membership.capability.id())?;
        let contacts = contacts_of(&membership, others.iter())?;
        let minted_by = membership.identity;
        let doc = stack.api.import_namespace(membership.capability).await?;
        stack.registry.register_pod(
            pod,
            PodBinding {
                membership: doc.clone(),
                records: None,
            },
        )?;
        stack.track(&doc, contacts, SyncStrategy::ContactsOnly, minted_by)?;
        let node = self.router.endpoint().id();
        let held = PodBinding {
            membership: doc.clone(),
            records: None,
        };
        derive_before_start(&stack, node, pod, &held).await?;
        self.watch_pod(&stack, pod, &doc);
        let contacts = stack
            .tracked(doc.id())?
            .map(|tracked| tracked.contacts)
            .unwrap_or_default();
        start_pod_store(&doc, contacts, minted_by, false).await
    }

    /// The pods `identity` holds here, each with whether its record store
    /// is held: `false` for a tombstone.
    pub fn pod_holdings(&self, identity: PdnId) -> Result<Vec<(PodId, bool)>> {
        let stack = self.require(identity)?;
        Ok(stack
            .registry
            .pods()?
            .into_iter()
            .map(|(pod, held)| (pod, held.records.is_some()))
            .collect())
    }

    /// At a departure: drop `pod`'s record store and keep its membership
    /// store, out of its swarm, as the tombstone. Finishes what an earlier
    /// forget left; [`UnknownPod`] for a pod `identity` never held.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn forget_pod(&self, identity: PdnId, pod: PodId) -> Result<()> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        if let Some(records) = held.records {
            // Drop first, as `forget_namespace` does: a failed drop leaves
            // the pod held, and a retry still finds it.
            self.forget_doc(identity, records.id()).await?;
            stack.registry.set_pod_records(pod, None)?;
        }
        // Before the leave, so the pass does not join the swarm again.
        stack.set_strategy(held.membership.id(), SyncStrategy::ContactsOnly)?;
        held.membership.leave_gossip().await
    }

    /// Reconcile the stores `identity` holds of `pod` — both, or the
    /// tombstone alone — with at most [`POD_RECONCILE_PEERS`] devices of its
    /// other members, drawn afresh, the membership store's exchange first.
    /// Returns once the dials are asked for; [`PodFlush::wait`] waits for
    /// their sessions and for the payloads of the identity's own entries.
    /// [`UnknownPod`] for a pod `identity` never held.
    pub async fn flush_pod(&self, identity: PdnId, pod: PodId) -> Result<PodFlush> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        let node = self.router.endpoint().id();
        let derived = pod_contacts(&stack, node, pod, &held, &[]).await?;
        let own = stack.identity();
        // A replica that lists nobody yet reaches the devices its tickets name.
        let contacts = match derived.contacts {
            Some(contacts) => contacts,
            None => stack
                .tracked(held.membership.id())?
                .map(|tracked| tracked.contacts)
                .unwrap_or_default(),
        };
        let others: Vec<Contact> = contacts
            .into_iter()
            .filter(|contact| contact.identity != own)
            .collect();
        let drawn = draw(others);
        let mut flush = PodFlush {
            since: std::time::SystemTime::now(),
            peers: drawn
                .iter()
                .map(|contact| {
                    (
                        NodeId::from_bytes(*contact.addr.id.as_bytes()),
                        contact.addr.clone(),
                    )
                })
                .collect(),
            sessions: Vec::new(),
            payloads: own_payloads(&stack, pod, &held).await?,
            endpoint: self.router.endpoint().clone(),
            blobs: self.blobs.clone(),
        };
        if drawn.is_empty() {
            return Ok(flush);
        }
        for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
            let Some(tracked) = stack.tracked(doc.id())? else {
                continue;
            };
            // Subscribed before the dial, so no session ends unseen.
            flush.sessions.push(Box::pin(doc.subscribe().await?));
            sync_pod_store(&stack, &tracked, drawn.clone()).await?;
        }
        Ok(flush)
    }

    /// The membership view of `identity`'s replica of `pod`'s membership
    /// store, payloads read as far as they have arrived.
    /// [`UnknownPod`] for a tombstone too.
    pub async fn pod_membership_view(&self, identity: PdnId, pod: PodId) -> Result<MembershipView> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        if held.records.is_none() {
            return Err(UnknownPod { pod }.into());
        }
        stack.access.membership_view(pod, &held.membership).await
    }

    /// What `identity` holds of `pod` here, a tombstone's membership view
    /// included; `None` for a pod not held here.
    pub async fn pod_holding(&self, identity: PdnId, pod: PodId) -> Result<Option<PodHolding>> {
        let stack = self.require(identity)?;
        let Some(held) = stack.registry.pod(pod)? else {
            return Ok(None);
        };
        Ok(Some(PodHolding {
            membership: stack.access.membership_view(pod, &held.membership).await?,
            records: held.records.is_some(),
        }))
    }

    /// The highest sequence of `subject`'s chain that `identity`'s replica
    /// of `pod` holds, a tombstone's included; `0` for a pod not held here.
    pub async fn pod_chain_run(&self, identity: PdnId, pod: PodId, subject: PdnId) -> Result<u64> {
        let stack = self.require(identity)?;
        let Some(held) = stack.registry.pod(pod)? else {
            return Ok(0);
        };
        let membership = stack.access.membership_view(pod, &held.membership).await?;
        Ok(membership.member(&subject).map_or(0, Member::run))
    }

    /// Wait, at most `timeout`, until the membership view of `identity`'s
    /// replica of `pod` lists `identity` itself as a member: a session
    /// brings its own entries ahead of their payloads. [`CatchUpTimeout`]
    /// when it does not.
    pub async fn await_pod_member(
        &self,
        identity: PdnId,
        pod: PodId,
        timeout: Duration,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        // Subscribed before the first membership view, so no change lands
        // unseen.
        let mut changes = held.membership.subscribe().await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let view = stack.access.membership_view(pod, &held.membership).await?;
            if view
                .member(&identity)
                .is_some_and(|member| member.state.member)
            {
                return Ok(());
            }
            match tokio::time::timeout_at(deadline, changes.next()).await {
                Ok(Some(_change)) => {}
                Ok(None) => anyhow::bail!("the membership store's change stream ended"),
                Err(_elapsed) => return Err(crate::CatchUpTimeout::default().into()),
            }
            // A burst — a session's entries, their payloads — is read once.
            while let Some(Some(_change)) = futures_lite::future::poll_once(changes.next()).await {}
        }
    }

    /// The record view over `identity`'s replica of `pod`'s record store,
    /// judged by the membership view of its membership store; payloads
    /// are checked for arrival, not read. [`UnknownPod`] for a tombstone
    /// too.
    pub async fn pod_record_view(&self, identity: PdnId, pod: PodId) -> Result<RecordView> {
        self.record_view(identity, pod, Query::all()).await
    }

    /// [`pod_record_view`](Self::pod_record_view) over `record`'s own
    /// entries alone.
    pub async fn pod_record_view_of(
        &self,
        identity: PdnId,
        pod: PodId,
        record: &RecordRef,
    ) -> Result<RecordView> {
        self.record_view(identity, pod, Query::key_prefix(record_prefix(record)))
            .await
    }

    /// A claim's or an immutable-document's payload: of the record's
    /// entries that read, the newest; `None` when none does.
    pub async fn read_pod_record(
        &self,
        identity: PdnId,
        pod: PodId,
        record: &RecordRef,
    ) -> Result<Option<Vec<u8>>> {
        let view = self.pod_record_view_of(identity, pod, record).await?;
        let Some(hash) = view.placed(record).and_then(|entry| entry.payload) else {
            return Ok(None);
        };
        Ok(Some(self.blobs.get_bytes(hash).await?.to_vec()))
    }

    /// A mergeable-document's operations that read, in the order of their
    /// ids.
    pub async fn read_pod_operations(
        &self,
        identity: PdnId,
        pod: PodId,
        record: &RecordRef,
    ) -> Result<Vec<Operation>> {
        let view = self.pod_record_view_of(identity, pod, record).await?;
        let mut operations = Vec::new();
        for (id, entry) in view.operations(record) {
            if let Some(hash) = entry.payload {
                let payload = self.blobs.get_bytes(hash).await?.to_vec();
                operations.push(Operation { id, payload });
            }
        }
        Ok(operations)
    }

    /// The entries of both of `pod`'s stores whose keys fit no layout of
    /// their store, each with its author. [`UnknownPod`] for a tombstone
    /// too.
    pub async fn list_pod_unknown(&self, identity: PdnId, pod: PodId) -> Result<Vec<UnknownEntry>> {
        let stack = self.require(identity)?;
        let mut unknown = Vec::new();
        for store in [PodStore::Membership, PodStore::Records] {
            let doc = Self::pod_doc(&stack, pod, store)?;
            unknown.extend(unknown_entries(&doc, store).await?);
        }
        Ok(unknown)
    }

    async fn record_view(
        &self,
        identity: PdnId,
        pod: PodId,
        query: impl Into<Query>,
    ) -> Result<RecordView> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        let records = held.records.ok_or(UnknownPod { pod })?;
        let membership = stack.access.membership_view(pod, &held.membership).await?;
        let entries = record_entries(&records, &self.blobs, query).await?;
        Ok(RecordView::new(&membership, entries))
    }

    /// Write `payload` at `key` into one of `pod`'s stores with
    /// `identity`'s author, checking nothing the key or the payload says:
    /// what the entry counts for is the membership view's and the record
    /// view's.
    /// [`UnknownPod`] for a tombstone too.
    ///
    /// A membership store's entry is on disk with its payload when this
    /// returns, and a commit that fails fails the call: the membership view
    /// rests on its payload, which no other device holds yet. A record
    /// store's lands in the stores' write batches, on disk within their
    /// settle window; [`flush_stores`](Self::flush_stores) first where
    /// anything durable outside the stores names what it wrote.
    pub async fn write_pod_entry(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
        doc.set_bytes(stack.author, key.to_vec(), payload.to_vec())
            .await?;
        if store == PodStore::Membership {
            stack.api.flush().await?;
            // Stated before the write returns, not once the change watch's
            // burst settles: an inviter pulls a newcomer's first write from a
            // device this write lists, and an unstated one is dialed as us.
            if let Some(held) = stack.registry.pod(pod)? {
                derive_pod_contacts(&stack, self.router.endpoint().id(), pod, &held, &[]).await;
            }
        }
        Ok(())
    }

    /// [`write_pod_entry`](Self::write_pod_entry) under `author`, an
    /// author `identity`'s replica store holds besides its own: an entry of
    /// a device no statement lists.
    #[cfg(feature = "test-util")]
    pub async fn write_pod_entry_as_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
        author: AuthorId,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
        doc.set_bytes(author, key.to_vec(), payload.to_vec())
            .await?;
        Ok(())
    }

    /// The contacts `identity`'s replica of one of `pod`'s stores is
    /// tracked with, as the last derivation left them; reading derives none.
    #[cfg(feature = "test-util")]
    pub fn pod_contacts_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
    ) -> Result<Vec<Contact>> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
        Ok(stack
            .tracked(doc.id())?
            .map(|tracked| tracked.contacts)
            .unwrap_or_default())
    }

    /// Taken before whatever starts the store's sessions, as
    /// [`PrivateMetadataStore::watch_catch_up`] is.
    pub async fn watch_pod_catch_up(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
    ) -> Result<crate::private_metadata::CatchUpWatch> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
        crate::private_metadata::watch_doc(&doc).await
    }

    fn pod_doc(stack: &HostedStack, pod: PodId, store: PodStore) -> Result<Doc> {
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        let records = held.records.ok_or(UnknownPod { pod })?;
        Ok(match store {
            PodStore::Membership => held.membership,
            PodStore::Records => records,
        })
    }

    /// Once; a second take yields `None`. From the take on, every membership
    /// view of a pod's membership store on this node — at a session's setup,
    /// at a read and at a run of the pod stores' pass — reports its verdicts.
    #[cfg(feature = "test-util")]
    pub fn take_pod_verdicts(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<crate::pod::PodVerdicts>> {
        let mut sender = self.pod_verdicts.lock().ok()?;
        if sender.is_some() {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *sender = Some(tx);
        Some(rx)
    }

    /// Once; a second take yields `None`. From the take on, every session
    /// decision of every identity this node hosts reports here, and every
    /// entry an allowed session's ingest admits.
    #[cfg(feature = "test-util")]
    pub fn take_session_traces(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<crate::access::SessionTrace>> {
        let mut sender = self.session_traces.lock().ok()?;
        if sender.is_some() {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *sender = Some(tx);
        Some(rx)
    }

    /// Reconcile one of `pod`'s stores with `contact` as a drawn contact
    /// is: through the engine, the record store after the membership
    /// store, a tombstone's membership store included. Returns once the
    /// dial is asked for, not once it ends, and joins no swarm, so a store
    /// taken out of one stays out.
    #[cfg(feature = "test-util")]
    pub async fn sync_pod_with_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
        contact: Contact,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = match store {
            PodStore::Membership => {
                stack
                    .registry
                    .pod(pod)?
                    .ok_or(UnknownPod { pod })?
                    .membership
            }
            PodStore::Records => Self::pod_doc(&stack, pod, store)?,
        };
        let tracked = stack
            .tracked(doc.id())?
            .context("the identity tracks no replica of that store")?;
        doc.sync_with_peers(vec![contact], Vec::new(), tracked.default_identity, false)
            .await
    }

    /// The exchanges of `pod`'s stores this node's identities have
    /// running, held behind another's, or waiting to redial; `0` once none
    /// of them can still dial on their own.
    #[cfg(feature = "test-util")]
    pub async fn pod_syncs_in_flight_for_test(&self, pod: PodId) -> Result<usize> {
        let stacks: Vec<Arc<HostedStack>> = self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("identities lock poisoned"))?
            .values()
            .cloned()
            .collect();
        let mut in_flight = 0_usize;
        for stack in stacks {
            let Some(held) = stack.registry.pod(pod)? else {
                continue;
            };
            let namespaces = std::iter::once(held.membership.id())
                .chain(held.records.as_ref().map(Doc::id))
                .collect();
            let running = stack.docs.engine().syncs_in_flight(namespaces).await?;
            in_flight = in_flight.saturating_add(running);
        }
        Ok(in_flight)
    }

    /// What [`Self::pod_syncs_in_flight_for_test`] counts, each item named.
    #[cfg(feature = "test-util")]
    pub async fn pod_syncs_in_flight_report_for_test(&self, pod: PodId) -> Result<String> {
        let stacks: Vec<Arc<HostedStack>> = self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("identities lock poisoned"))?
            .values()
            .cloned()
            .collect();
        let mut report = Vec::new();
        for stack in stacks {
            let Some(held) = stack.registry.pod(pod)? else {
                continue;
            };
            let namespaces = std::iter::once(held.membership.id())
                .chain(held.records.as_ref().map(Doc::id))
                .collect();
            report.push(
                stack
                    .docs
                    .engine()
                    .syncs_in_flight_report(namespaces)
                    .await?,
            );
        }
        Ok(report.join("; "))
    }

    /// While `refuse`, every session on a pod's store that `identity` is
    /// asked to serve is refused, as by a device out of reach; its own dials
    /// go on.
    #[cfg(feature = "test-util")]
    pub fn refuse_pod_sessions_for_test(&self, identity: PdnId, refuse: bool) -> Result<()> {
        self.require(identity)?.access.refuse_pod_sessions(refuse);
        Ok(())
    }

    /// While `serve`, every session on a pod's store that `identity` is
    /// asked to serve is served whole, as by a modified node that judges no
    /// caller; its own dials are judged as ever.
    #[cfg(feature = "test-util")]
    pub fn serve_pod_sessions_whole_for_test(&self, identity: PdnId, serve: bool) -> Result<()> {
        self.require(identity)?
            .access
            .serve_pod_sessions_whole(serve);
        Ok(())
    }

    /// [`pod_membership_view`](Self::pod_membership_view) over a tombstone as
    /// well.
    #[cfg(feature = "test-util")]
    pub async fn held_pod_membership_view_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
    ) -> Result<MembershipView> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        stack.access.membership_view(pod, &held.membership).await
    }

    /// Every session one of `pod`'s stores finishes from now on, dialed or
    /// accepted, with what it exchanged. [`UnknownPod`] for a tombstone
    /// too.
    #[cfg(feature = "test-util")]
    pub async fn watch_pod_sessions(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
    ) -> Result<PodSessions> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
        Ok(PodSessions {
            events: Box::pin(doc.subscribe().await?),
        })
    }

    /// Once; a second take yields `None`. From the take on, every run of the
    /// pod stores' pass reports whom it reached for each store.
    #[cfg(feature = "test-util")]
    pub fn take_pod_pass_draws(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<PodPassDraw>> {
        let mut sender = self.pass_probes.draws.lock().ok()?;
        if sender.is_some() {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *sender = Some(tx);
        Some(rx)
    }

    /// Whether the node's blob store holds the payload `hash` names.
    #[cfg(feature = "test-util")]
    pub async fn holds_payload(&self, hash: Hash) -> Result<bool> {
        Ok(self.blobs.has(hash).await?)
    }

    /// Put `bytes` into the node's blob store referenced by no replica: the
    /// payload the next collection run removes.
    #[cfg(feature = "test-util")]
    pub async fn add_stray_payload_for_test(&self, bytes: &[u8]) -> Result<Hash> {
        let tag = self.blobs.add_bytes(bytes.to_vec()).temp_tag().await?;
        Ok(tag.hash())
    }

    /// The runs of the pass over every tracked store but a pod's finished
    /// since spawn.
    #[cfg(feature = "test-util")]
    pub fn reconcile_passes(&self) -> u64 {
        self.pass_probes
            .passes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The namespaces of `pod`'s membership store and record store as
    /// `identity` holds them. [`UnknownPod`] for a tombstone too.
    pub fn pod_stores(&self, identity: PdnId, pod: PodId) -> Result<(NamespaceId, NamespaceId)> {
        let held = self
            .require(identity)?
            .registry
            .pod(pod)?
            .ok_or(UnknownPod { pod })?;
        let records = held.records.ok_or(UnknownPod { pod })?;
        Ok((held.membership.id(), records.id()))
    }

    /// The created event `identity`'s replica of `pod` counts; `None` while
    /// it counts none. [`UnknownPod`] for a pod never held here.
    pub async fn pod_creation(
        &self,
        identity: PdnId,
        pod: PodId,
    ) -> Result<Option<CreatedPayload>> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        let (view, entries) = stack
            .access
            .membership_view_and_entries(pod, &held.membership)
            .await?;
        Ok(entries
            .iter()
            .zip(view.verdicts())
            .find(|(entry, verdict)| {
                **verdict == Verdict::Counted
                    && matches!(
                        MembershipKey::parse(&entry.key),
                        Some(MembershipKey::Event {
                            kind: EventKind::Created,
                            ..
                        })
                    )
            })
            .and_then(|(entry, _verdict)| entry.payload.bytes().and_then(CreatedPayload::decode)))
    }

    /// Minted with no restart of either store's sync: a restart dials every
    /// peer the store recorded, a former member's device among them.
    /// [`UnknownPod`] for a tombstone too.
    pub async fn share_pod_tickets(
        &self,
        identity: PdnId,
        pod: PodId,
        addr_options: AddrInfoOptions,
    ) -> Result<PodTickets> {
        let held = self
            .require(identity)?
            .registry
            .pod(pod)?
            .ok_or(UnknownPod { pod })?;
        let records = held.records.ok_or(UnknownPod { pod })?;
        Ok(PodTickets {
            membership: held.membership.ticket(addr_options).await?,
            records: records.ticket(addr_options).await?,
        })
    }

    /// The addresses this node's endpoint knows for `nodes`, each as a ticket
    /// names a node; this node, and a node it knows no address of, left out.
    pub async fn known_addrs(&self, nodes: &[NodeId]) -> Vec<EndpointAddr> {
        let endpoint = self.router.endpoint();
        let mut known = Vec::new();
        for node in nodes {
            let Ok(id) = EndpointId::from_bytes(node.as_bytes()) else {
                continue;
            };
            if id == endpoint.id() || known.iter().any(|addr: &EndpointAddr| addr.id == id) {
                continue;
            }
            if let Some(info) = endpoint.remote_info(id).await {
                let addrs: Vec<iroh::TransportAddr> = info.into_addrs().map(Into::into).collect();
                if !addrs.is_empty() {
                    known.push(
                        AddrInfoOptions::RelayAndAddresses
                            .apply(&EndpointAddr::from_parts(id, addrs)),
                    );
                }
            }
        }
        known
    }

    /// `identity`'s own write tickets to the two stores `tickets` name, this
    /// device their one node as a share would name it, minted with no replica
    /// held: a join records them before it imports the stores.
    pub fn own_pod_tickets(&self, identity: PdnId, tickets: &PodTickets) -> Result<PodTickets> {
        let own = self.require(identity)?.identity();
        let node = AddrInfoOptions::RelayAndAddresses.apply(&self.router.endpoint().addr());
        let mint = |ticket: &DocTicket| DocTicket {
            capability: ticket.capability.clone(),
            nodes: vec![node.clone()],
            identity: own,
        };
        Ok(PodTickets {
            membership: mint(&tickets.membership),
            records: mint(&tickets.records),
        })
    }

    /// Once; a second take yields `None`. From the take on, every
    /// derivation of a pod's contacts — at each change to its membership
    /// store and each run of the pod stores' pass, ahead of its dials —
    /// reports what it finds, until the record store is forgotten.
    pub fn take_pod_notices(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<PodNotice>> {
        let mut sender = self.pod_notices.lock().ok()?;
        if sender.is_some() {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *sender = Some(tx);
        Some(rx)
    }

    /// A fresh doc of `identity`'s for a device-shared store, tracked.
    pub(crate) async fn new_doc(&self, identity: PdnId) -> Result<Doc> {
        let stack = self.require(identity)?;
        let doc = stack.api.create().await?;
        stack.track(&doc, Vec::new(), SyncStrategy::Swarm, stack.identity())?;
        Ok(doc)
    }

    /// Recovery's counterpart of `new_doc` / `import_doc`: a namespace the
    /// identity's store does not hold is `Ok(None)`, kept apart from a
    /// store that could not answer.
    pub(crate) async fn open_doc(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
    ) -> Result<Option<Doc>> {
        let stack = self.require(identity)?;
        if let Some((pod, _store)) = stack.registry.pod_of(namespace)? {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a store of pod {pod}; \
                 it cannot be opened as a device-shared store"
            ));
        }
        let Some(doc) = stack
            .api
            .open(namespace)
            .await
            .with_context(|| format!("namespace {namespace} did not open"))?
        else {
            return Ok(None);
        };
        stack.track(&doc, Vec::new(), SyncStrategy::Swarm, stack.identity())?;
        Ok(Some(doc))
    }

    /// Read both stores, to tell stores that still answer from one a full
    /// filesystem left refusing everything. The replica store is asked one
    /// replica's sync peers because that reaches the tables; the namespace
    /// listing and an empty entry query both keep answering long after the
    /// database has refused everything else. The blob store is asked for a
    /// hash other than the empty one, which it answers without asking its
    /// metadata actor: a placement that meets the full disk stops that
    /// actor and leaves the replica store answering. Each identity's stores
    /// are flushed too, which answers a commit their own timer saw fail.
    pub async fn check_stores(&self) -> Result<()> {
        let _held = self.blobs.has(Hash::from_bytes([0; 32])).await?;
        for stack in self.stacks()? {
            stack.api.flush().await?;
            if let Some(tracked) = stack.tracked_snapshot().first() {
                let _peers = tracked.doc.get_sync_peers().await?;
            } else {
                let mut listed = stack.api.list().await?;
                if let Some(entry) = listed.next().await {
                    let _first = entry?;
                }
            }
        }
        Ok(())
    }

    /// Import a device-shared store's doc for `identity`, tracked with the
    /// ticket's contacts.
    pub(crate) async fn import_doc(&self, identity: PdnId, ticket: DocTicket) -> Result<Doc> {
        let stack = self.require(identity)?;
        Self::guard_shared_import(&stack, ticket.capability.id())?;
        let contacts = ticket.contacts();
        let minted_by = ticket.identity;
        // The capability only: a session started before the store is armed
        // is refused by this identity's own book, and nothing retries it
        // before the next pass. `host_identity` starts the sync once the
        // store is armed.
        let doc = stack.api.import_namespace(ticket.capability).await?;
        stack.track(&doc, contacts, SyncStrategy::Swarm, minted_by)?;
        Ok(doc)
    }

    /// Untrack and drop a device-shared store's doc. A pod's stores go
    /// through [`forget_pod`](Self::forget_pod).
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn forget_doc(&self, identity: PdnId, namespace: NamespaceId) -> Result<()> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(());
        };
        // Drop first: untracked before a drop that fails, the replica stays
        // open and served but leaves the reconcile pass, and nothing
        // counted says so.
        stack.api.drop_doc(namespace).await?;
        stack.untrack(namespace)?;
        Ok(())
    }

    /// Make every write of `identity` so far durable — its replica store's
    /// entries and the payloads they hold in the node's blob store — and
    /// answer the error of a commit that failed. To call before anything
    /// durable outside the stores names what was written: the stores
    /// otherwise commit within their settle window, a failure only logged.
    pub async fn flush_stores(&self, identity: PdnId) -> Result<()> {
        self.require(identity)?.api.flush().await
    }

    /// How many entries this device wrote into `identity`'s PMS and into
    /// the membership stores of its pods whose payload the blob store does
    /// not hold: each a write a kill cut within the stores' settle window
    /// with no flush behind it, which what rests on it waits for forever.
    /// Zero while every such write is flushed before it returns.
    pub async fn own_entries_without_payload(&self, identity: PdnId) -> Result<usize> {
        let stack = self.require(identity)?;
        let mut docs: Vec<Doc> = stack.access.armed_pms()?.into_iter().collect();
        docs.extend(
            stack
                .registry
                .pods()?
                .into_iter()
                .map(|(_pod, held)| held.membership),
        );
        let mut missing = 0usize;
        for doc in docs {
            missing = missing
                .saturating_add(entries_without_payload(&doc, stack.author, &self.blobs).await?);
        }
        Ok(missing)
    }

    pub(crate) fn blobs(&self) -> iroh_blobs::api::Store {
        self.blobs.clone()
    }

    /// A standalone author; an identity's own stores write with
    /// [`default_author`](Self::default_author) instead.
    ///
    /// Lands in the stores' write batches, on disk within their settle
    /// window; [`flush_stores`](Self::flush_stores) first where anything durable outside the
    /// stores names what it wrote.
    pub async fn create_author(&self, identity: PdnId) -> Result<AuthorId> {
        let author = self.require(identity)?.api.author_create().await?;
        Ok(author)
    }

    /// The identity's one author, persisted with its replicas. An
    /// author minted per store or per start would make a rewritten key
    /// accumulate one live record per author, and leave a device record
    /// written under one author standing after a withdrawal written under
    /// another.
    pub fn default_author(&self, identity: PdnId) -> Result<AuthorId> {
        Ok(self.require(identity)?.author)
    }

    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(*self.router.endpoint().id().as_bytes())
    }

    pub fn dial_handle(&self) -> DialHandle {
        DialHandle {
            endpoint: self.router.endpoint().clone(),
        }
    }

    /// Whether `identity` still holds `namespace` in its replica store or
    /// under the reconcile pass; an identity not hosted holds nothing.
    #[cfg(feature = "test-util")]
    pub async fn holds_replica(&self, identity: PdnId, namespace: NamespaceId) -> Result<bool> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(false);
        };
        Ok(stack.tracked(namespace)?.is_some() || holds_namespace(&stack.api, namespace).await?)
    }

    /// The identities this node hosts.
    pub fn hosted_identities(&self) -> Result<Vec<PdnId>> {
        Ok(self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?
            .keys()
            .copied()
            .collect())
    }

    /// Let blob collection run on a directory-configured node, once the
    /// host has hosted again every identity it means to: a run before that
    /// would remove the payloads of an identity recovery has yet to reach.
    /// A node in memory never collects.
    pub fn start_blob_collection(&self) {
        self.collecting
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Idempotent under a shared reference.
    pub async fn shutdown(&self) -> Result<()> {
        // First, so it does not race the docs engines' shutdown with fresh
        // sync requests.
        if let Some(stop) = self
            .reconciler_stop
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        {
            let _ = stop.send(());
        }
        // While the endpoint still answers for its peers.
        if let Some(book) = &self.address_book {
            let peers = book_peers(&self.identities, self.router.endpoint().id()).await;
            if let Err(err) = book.refresh(self.router.endpoint(), peers).await {
                tracing::warn!("the address book was not saved at shutdown: {err:#}");
            }
        }
        let directory_lock = self
            .directory_lock
            .lock()
            .ok()
            .and_then(|mut held| held.take());
        stop_node(
            &self.identities,
            &self.router,
            &self.blob_streams,
            &self.blobs,
            &self.blob_store_closed,
            directory_lock,
            #[cfg(feature = "test-util")]
            &self.stop_steps,
        )
        .await
    }

    fn stack(&self, identity: PdnId) -> Result<Option<Arc<HostedStack>>> {
        Ok(self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?
            .get(&identity)
            .cloned())
    }

    fn stacks(&self) -> Result<Vec<Arc<HostedStack>>> {
        Ok(self
            .identities
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?
            .values()
            .cloned()
            .collect())
    }

    fn require(&self, identity: PdnId) -> Result<Arc<HostedStack>> {
        self.stack(identity)?
            .ok_or_else(|| IdentityNotProvisioned { identity }.into())
    }
}

/// `identity` has no half of this node: nothing was provisioned for it, or
/// it was dropped. Downcast from the `anyhow::Error` of the
/// identity-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no stores are provisioned for identity {identity} on this node")]
pub struct IdentityNotProvisioned {
    pub identity: PdnId,
}

/// The identities whose subdirectory holds a hosting record, plus
/// `opening` when its own does not yet: what a store's share of the cache
/// budget is cut from, so no host states a count of its own (ADR-0013).
fn hosted_identity_count(directory: &std::path::Path, opening: PdnId) -> Result<usize> {
    let identities = directory.join(IDENTITIES_DIR);
    let context = || {
        format!(
            "cannot read the identities directory {}",
            identities.display()
        )
    };
    let own = identity_directory(directory, opening);
    let mut recorded = 0usize;
    let mut opening_recorded = false;
    for entry in std::fs::read_dir(&identities).with_context(context)? {
        let entry = entry.with_context(context)?;
        if entry.path().join(HOSTING_RECORD_FILE).is_file() {
            recorded = recorded.saturating_add(1);
            opening_recorded |= entry.path() == own;
        }
    }
    Ok(if opening_recorded {
        recorded
    } else {
        recorded.saturating_add(1)
    })
}

fn write_hosting_record(own: &std::path::Path, pms: NamespaceId) -> Result<()> {
    use std::io::Write as _;
    let path = own.join(HOSTING_RECORD_FILE);
    let staged = own.join(format!("{HOSTING_RECORD_FILE}.tmp"));
    let context = || format!("cannot write the hosting record {}", path.display());
    {
        let mut file = std::fs::File::create(&staged).with_context(context)?;
        file.write_all(pms.to_string().as_bytes())
            .with_context(context)?;
        // A rename can commit before the data reaches the disk.
        file.sync_all().with_context(context)?;
    }
    std::fs::rename(&staged, &path).with_context(context)
}

fn read_hosting_records(directory: &std::path::Path) -> Result<Vec<RecordedHosting>> {
    let identities = directory.join(IDENTITIES_DIR);
    let context = || {
        format!(
            "cannot read the identities directory {}",
            identities.display()
        )
    };
    let listing = match std::fs::read_dir(&identities) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(context),
    };
    let mut recorded = Vec::new();
    for entry in listing {
        let entry = entry.with_context(context)?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(identity) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<PdnId>().ok())
        else {
            tracing::warn!(
                entry = %entry.path().display(),
                "a subdirectory of the identities directory names no identity; left alone"
            );
            continue;
        };
        let path = entry.path().join(HOSTING_RECORD_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot read the hosting record {}", path.display()))
            }
        };
        let pms = text
            .trim()
            .parse::<NamespaceId>()
            .with_context(|| format!("cannot parse the hosting record {}", path.display()))?;
        let store_present = entry.path().join(REPLICA_STORE_FILE).is_file();
        recorded.push(RecordedHosting {
            identity,
            pms,
            store_present,
        });
    }
    Ok(recorded)
}

/// One subdirectory per hosted identity, named by the identity, each
/// holding that identity's replica store and author.
fn identity_directory(directory: &std::path::Path, identity: PdnId) -> std::path::PathBuf {
    directory
        .join(IDENTITIES_DIR)
        .join(encode_hex(identity.as_bytes()))
}

/// Answered from the listing: opening a replica holds it open.
#[cfg(feature = "test-util")]
async fn holds_namespace(api: &DocsApi, namespace: NamespaceId) -> Result<bool> {
    let mut listed = api.list().await?;
    while let Some(entry) = listed.next().await {
        let (id, _capability) = entry?;
        if id == namespace {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What the engines ask about the node's other identities waits here: a
/// full channel drops a request, which the periodic co-located pass makes
/// up for.
const CO_LOCATED_REQUESTS_CAPACITY: usize = 1024;

/// Carry out what the engines ask about the node's other identities. The
/// identity map is held weakly: the engines hold the other end of the
/// channel, so a strong hold would keep the node alive through its own
/// engines, and the task ends once the last of them is gone.
async fn serve_co_located(
    identities: std::sync::Weak<std::sync::RwLock<HashMap<PdnId, Arc<HostedStack>>>>,
    mut requests: tokio::sync::mpsc::Receiver<pdn_store::engine::CoLocatedRequest>,
) {
    use pdn_store::engine::CoLocatedRequest;
    while let Some(request) = requests.recv().await {
        let Some(identities) = identities.upgrade() else {
            return;
        };
        match request {
            CoLocatedRequest::Announce { namespace, writer } => {
                reconcile_with_co_located(&identities, namespace, writer, None);
            }
            CoLocatedRequest::Dial {
                namespace,
                caller,
                callee,
            } => reconcile_with_co_located(&identities, namespace, caller, Some(callee)),
        }
    }
}

/// Reconcile `namespace` between `source` and the identities of this same
/// node that hold it — `callee` alone when a contact named one, every
/// other identity of it when a write announces. Content-free either way:
/// what the receiving identity obtains comes through the session and its
/// filter.
fn reconcile_with_co_located(
    identities: &Identities,
    namespace: NamespaceId,
    source: Identity,
    callee: Option<Identity>,
) {
    let Ok(hosted) = identities.read() else {
        return;
    };
    let Some(from) = hosted.values().find(|stack| stack.identity() == source) else {
        return;
    };
    let from = Arc::clone(from);
    let first = governing_store(&from, namespace);
    let targets: Vec<Arc<HostedStack>> = hosted
        .values()
        .filter(|stack| stack.identity() != source)
        .filter(|stack| callee.is_none_or(|callee| stack.identity() == callee))
        .filter(|stack| matches!(stack.tracked(namespace), Ok(Some(_))))
        .cloned()
        .collect();
    drop(hosted);
    for target in targets {
        let identity = target.identity();
        {
            let Ok(mut in_flight) = from.announcements_in_flight.lock() else {
                continue;
            };
            if !in_flight.insert((namespace, identity)) {
                continue;
            }
        }
        let from = Arc::clone(&from);
        let _detached = tokio::spawn(async move {
            // The engine holds the record store's dial until this exchange
            // of the membership store finishes.
            if let Some(first) = first {
                let _ = from
                    .docs
                    .engine()
                    .sync_in_process(target.docs.engine(), first, identity)
                    .await;
            }
            let _ = from
                .docs
                .engine()
                .sync_in_process(target.docs.engine(), namespace, identity)
                .await;
            if let Ok(mut in_flight) = from.announcements_in_flight.lock() {
                in_flight.remove(&(namespace, identity));
            }
        });
    }
}

/// The membership store of the pod whose record store `namespace` is,
/// which its every reconciliation follows.
fn governing_store(stack: &HostedStack, namespace: NamespaceId) -> Option<NamespaceId> {
    let Ok(Some((pod, PodStore::Records))) = stack.registry.pod_of(namespace) else {
        return None;
    };
    Some(stack.registry.pod(pod).ok()??.membership.id())
}

/// The node's one blob store, collected at the interval `options` sets
/// once `collecting` is set; a node in memory's is never collected.
async fn open_blob_store(
    options: &SpawnOptions,
    identities: &Identities,
    collecting: &Arc<std::sync::atomic::AtomicBool>,
    closed: &Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-util")] pause: &CollectionPauseSlot,
    #[cfg(feature = "test-util")] runs: &Arc<std::sync::atomic::AtomicUsize>,
) -> Result<iroh_blobs::api::Store> {
    Ok(match &options.storage {
        #[cfg(any(test, feature = "test-util"))]
        StorageConfig::Memory => iroh_blobs::store::mem::MemStore::new().into(),
        StorageConfig::Directory(directory) => {
            let root = directory.join(BLOBS_DIR);
            let mut store_options = iroh_blobs::store::fs::options::Options::new(&root);
            let opened = Arc::new(std::sync::OnceLock::new());
            store_options.gc = Some(GcConfig {
                interval: options.blob_collection_interval.min(BLOB_COLLECTION_TICK),
                add_protected: Some(protect_hosted(
                    Arc::downgrade(identities),
                    Arc::clone(collecting),
                    Arc::clone(closed),
                    Arc::clone(&opened),
                    options.blob_collection_interval,
                    #[cfg(feature = "test-util")]
                    Arc::clone(pause),
                    #[cfg(feature = "test-util")]
                    Arc::clone(runs),
                )),
            });
            let store: iroh_blobs::api::Store =
                FsStore::load_with_opts(root.join("blobs.db"), store_options)
                    .await
                    .with_context(|| {
                        format!("cannot open the blob store in {}", directory.display())
                    })?
                    .into();
            let _ = opened.set(store.clone());
            store
        }
    })
}

/// Blob collection's protect callback: every payload a replica of an
/// identity the node hosts references, the node's one blob store being
/// every identity's. A run goes ahead once `interval` has passed since the
/// last and `collecting` is set; every other ask is skipped. Once the
/// node's stop has closed the store (`closed`), or the node is gone with no
/// stop to close it, the callback lets the next run fail on the closed
/// store: iroh-blobs ends the collection's task on a failed run alone, and
/// the task holds the store's runtime while it lives
/// (`components/mee-pdn/iroh-defects.md`).
fn protect_hosted(
    identities: std::sync::Weak<std::sync::RwLock<HashMap<PdnId, Arc<HostedStack>>>>,
    collecting: Arc<std::sync::atomic::AtomicBool>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    store: Arc<std::sync::OnceLock<iroh_blobs::api::Store>>,
    interval: Duration,
    #[cfg(feature = "test-util")] pause: CollectionPauseSlot,
    #[cfg(feature = "test-util")] runs: Arc<std::sync::atomic::AtomicUsize>,
) -> ProtectCb {
    let next_run = Arc::new(Mutex::new(std::time::Instant::now() + interval));
    Arc::new(move |live: &mut HashSet<Hash>| {
        #[cfg(feature = "test-util")]
        runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let identities = identities.clone();
        let collecting = Arc::clone(&collecting);
        let closed = Arc::clone(&closed);
        let store = Arc::clone(&store);
        let next_run = Arc::clone(&next_run);
        #[cfg(feature = "test-util")]
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            let Some(identities) = identities.upgrade() else {
                // Dropped outside a tokio runtime, so no stop closed the
                // store: it is closed here.
                if !closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    if let Some(store) = store.get().cloned() {
                        // A task of its own: the callback's future has to
                        // be `Sync`, which the store's calls are not.
                        let _ = tokio::spawn(async move { store.shutdown().await }).await;
                    }
                }
                return last_collection_run(&store).await;
            };
            if closed.load(std::sync::atomic::Ordering::Acquire) {
                return last_collection_run(&store).await;
            }
            let now = std::time::Instant::now();
            match next_run.lock() {
                Ok(due) if now >= *due => {}
                _ => return ProtectOutcome::Abort,
            }
            if !collecting.load(std::sync::atomic::Ordering::Acquire) {
                return ProtectOutcome::Abort;
            }
            let stacks: Vec<Arc<HostedStack>> = match identities.read() {
                Ok(hosted) => hosted.values().cloned().collect(),
                Err(_poisoned) => return ProtectOutcome::Abort,
            };
            for stack in stacks {
                let Ok(referenced) = referenced_payloads(&stack).await else {
                    return ProtectOutcome::Abort;
                };
                live.extend(referenced);
            }
            #[cfg(feature = "test-util")]
            {
                let pause = pause.lock().ok().and_then(|mut slot| slot.take());
                if let Some(pause) = pause {
                    pause.reached.notify_one();
                    pause.release.notified().await;
                }
            }
            if let Ok(mut due) = next_run.lock() {
                *due = now + interval;
            }
            ProtectOutcome::Continue
        })
    })
}

/// The answer that ends the collection's task once the node's stop closed
/// the store: iroh-blobs drops the store's metadata database before its
/// shutdown answers, and a run's every call goes there first, so the run
/// fails before it sweeps anything. A store that still answers is left to a
/// skipped run instead.
async fn last_collection_run(
    store: &std::sync::OnceLock<iroh_blobs::api::Store>,
) -> ProtectOutcome {
    let Some(store) = store.get().cloned() else {
        return ProtectOutcome::Abort;
    };
    match tokio::spawn(async move { store.sync_db().await.is_err() }).await {
        Ok(true) => ProtectOutcome::Continue,
        _ => ProtectOutcome::Abort,
    }
}

/// Every payload hash the replicas of one identity's store reference.
async fn referenced_payloads(stack: &HostedStack) -> Result<Vec<Hash>> {
    stack.docs.engine().collection_hashes().await
}

/// One subdirectory per identity, each holding that identity's replica
/// store (`docs.redb`), its persisted author (`default-author`) and, once
/// a create or link commits, its hosting record (`pms`).
const IDENTITIES_DIR: &str = "identities";
/// The store pdn-store opens in an identity's subdirectory.
const REPLICA_STORE_FILE: &str = "docs.redb";
/// The namespace of the identity's PMS, as text: a
/// start hosts exactly the identities whose subdirectory holds one.
const HOSTING_RECORD_FILE: &str = "pms";
const BLOBS_DIR: &str = "blobs";
/// The endpoint's secret key, hex-encoded.
const NODE_KEY_FILE: &str = "node.key";
/// The running node's exclusive hold, content-free.
const LOCK_FILE: &str = "lock";

/// The replica store holds namespace secrets and the blob store payload
/// bytes in the clear, so the boundary sits on the directory.
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const KEY_MODE: u32 = 0o600;

/// Create the directory owner-only when absent; one that exists — a mounted
/// volume — is taken as the caller gave it.
fn provision_directory(directory: &std::path::Path) -> Result<()> {
    if !directory.exists() {
        create_owner_only_dir(directory)?;
    }
    for sub in [IDENTITIES_DIR, BLOBS_DIR] {
        std::fs::create_dir_all(directory.join(sub)).with_context(|| {
            format!(
                "cannot create {sub}/ in storage directory {}",
                directory.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_owner_only_dir(directory: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(directory)
        .with_context(|| format!("cannot create storage directory {}", directory.display()))?;
    // Checked, not assumed: the umask can strip bits at creation.
    let mode = std::fs::metadata(directory)?.permissions().mode() & 0o777;
    if mode != DIR_MODE {
        return Err(anyhow::anyhow!(
            "storage directory {} was created with permissions {mode:o}, expected {DIR_MODE:o}",
            directory.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_owner_only_dir(directory: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create storage directory {}", directory.display()))?;
    Ok(())
}

/// An advisory lock on the `lock` file, taken before the stores open their
/// own: the refusal then names the directory rather than reading as
/// corruption, and the blob store's open on a database another node holds
/// waits instead of failing.
fn lock_directory(directory: &std::path::Path) -> Result<std::fs::File> {
    let path = directory.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open the lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(anyhow::Error::new(DirectoryHeld {
            directory: directory.to_path_buf(),
        })),
        Err(std::fs::TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("cannot lock the lock file {}", path.display()))
        }
    }
}

/// A key file present but unreadable stops the start — never a regenerated
/// key, which would silently change the node id.
fn read_or_generate_node_key(directory: &std::path::Path) -> Result<SecretKey> {
    let path = directory.join(NODE_KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_node_key(&text)
            .with_context(|| format!("cannot parse the node key file {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            generate_node_key(directory, &path)
        }
        Err(err) => {
            Err(err).with_context(|| format!("cannot read the node key file {}", path.display()))
        }
    }
}

fn parse_node_key(text: &str) -> Result<SecretKey> {
    text.trim()
        .parse::<SecretKey>()
        .map_err(|err| anyhow::anyhow!("not a secret key: {err}"))
}

/// Lowercase hex, the encoding `SecretKey`'s own parser accepts back.
fn encode_hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Written beside and linked into place, so no half-written key can exist;
/// linked exclusively, so two starts racing on one directory cannot mint
/// different keys — the loser reads the winner's file.
#[cfg(unix)]
fn generate_node_key(directory: &std::path::Path, path: &std::path::Path) -> Result<SecretKey> {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let fresh = SecretKey::generate();
    let encoded = encode_hex(&fresh.to_bytes());
    let staged = directory.join(format!("{NODE_KEY_FILE}.tmp"));
    // A leftover from a start interrupted mid-write must not stop every
    // later one; removed rather than truncated so the mode below is the
    // file's own. Safe under the directory's exclusive lock.
    let _leftover_gone = std::fs::remove_file(&staged);
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(KEY_MODE)
            .open(&staged)
            .with_context(|| format!("cannot stage the node key beside {}", path.display()))?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
    }
    let committed = match std::fs::hard_link(&staged, path) {
        Ok(()) => Ok(fresh),
        // Another start committed first; its key is the node's key.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the node key file {}", path.display()))?;
            parse_node_key(&text)
                .with_context(|| format!("cannot parse the node key file {}", path.display()))
        }
        Err(err) => {
            Err(err).with_context(|| format!("cannot commit the node key file {}", path.display()))
        }
    };
    let _staged_gone = std::fs::remove_file(&staged);
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != KEY_MODE {
        return Err(anyhow::anyhow!(
            "node key file {} has permissions {mode:o}, expected {KEY_MODE:o}",
            path.display()
        ));
    }
    committed
}

#[cfg(not(unix))]
fn generate_node_key(_directory: &std::path::Path, path: &std::path::Path) -> Result<SecretKey> {
    use std::io::Write;
    let fresh = SecretKey::generate();
    let encoded = encode_hex(&fresh.to_bytes());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create the node key file {}", path.display()))?;
    file.write_all(encoded.as_bytes())?;
    file.sync_all()?;
    Ok(fresh)
}

/// Name the directory in a failed store open, and reclassify a replica
/// store held by another node as [`DirectoryHeld`] rather than a lock error
/// that reads as corruption. The underlying error stays in the chain.
fn annotate_store_error(err: anyhow::Error, storage: &StorageConfig) -> anyhow::Error {
    let Some(directory) = storage.directory() else {
        return err;
    };
    let held = err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<redb::DatabaseError>(),
            Some(redb::DatabaseError::DatabaseAlreadyOpen)
        )
    });
    if held {
        err.context(DirectoryHeld {
            directory: directory.to_path_buf(),
        })
    } else {
        err.context(format!(
            "cannot open the node's stores in {}",
            directory.display()
        ))
    }
}

/// Provision the directory, take its lock, and read or mint the key —
/// the lock first, so one running node per directory is refused by name.
/// One `spawn_blocking` step: the `std::fs` calls and two `sync_all`s stall
/// a worker thread on a virtualized filesystem. The open file holds the
/// lock, not the thread that opened it.
async fn prepare_storage(
    storage: &StorageConfig,
) -> Result<(
    Option<SecretKey>,
    Option<std::fs::File>,
    Option<Arc<AddressBook>>,
)> {
    let Some(directory) = storage.directory() else {
        return Ok((None, None, None));
    };
    let directory = directory.to_path_buf();
    let (key, lock, book) = tokio::task::spawn_blocking(move || {
        provision_directory(&directory)?;
        let lock = lock_directory(&directory)?;
        let key = read_or_generate_node_key(&directory)?;
        let book = AddressBook::open(&directory);
        anyhow::Ok((key, lock, book))
    })
    .await
    .context("the storage directory could not be provisioned")??;
    Ok((Some(key), Some(lock), Some(Arc::new(book))))
}

/// If `PDN_BIND_ADDR` holds an IP address the endpoint binds it with an
/// ephemeral port (the just recipes set `127.0.0.1` to keep test traffic on
/// loopback); unset, all interfaces. Only the widest `connectivity` takes
/// iroh's `N0` preset, which publishes a record under this node's id; the
/// narrower two take the relay mode alone over `Minimal`.
async fn bind_endpoint(
    secret_key: Option<SecretKey>,
    connectivity: Connectivity,
    address_book: Option<iroh::address_lookup::MemoryLookup>,
) -> Result<Endpoint> {
    let builder = match connectivity {
        Connectivity::Direct => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        Connectivity::Relays => {
            Endpoint::builder(presets::Minimal).relay_mode(default_relay_mode())
        }
        Connectivity::RelaysAndAddressLookup => Endpoint::builder(presets::N0),
    };
    let builder = builder
        .path_selector(std::sync::Arc::new(
            crate::path_selector::ReceivingPathSelector,
        ))
        .transport_config(
            QuicTransportConfig::builder()
                .max_concurrent_uni_streams(VarInt::from_u32(MAX_UNI_STREAMS))
                .receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW))
                .build(),
        );
    let builder = match secret_key {
        Some(key) => builder.secret_key(key),
        None => builder,
    };
    let builder = match address_book {
        Some(book) => builder.address_lookup(book),
        None => builder,
    };
    let builder = match std::env::var("PDN_BIND_ADDR") {
        Ok(addr) if !addr.is_empty() => {
            let ip: IpAddr = addr
                .parse()
                .context("PDN_BIND_ADDR must be an IP address")?;
            builder.bind_addr((ip, 0u16))?
        }
        _ => builder,
    };
    let endpoint = builder.bind().await?;
    wait_until_dialable(&endpoint).await;
    Ok(endpoint)
}

/// Refresh the address book every [`ADDRESS_BOOK_INTERVAL`] until the
/// node stops; `shutdown` takes the last refresh itself.
async fn keep_address_book(
    book: Arc<AddressBook>,
    endpoint: Endpoint,
    identities: Identities,
    mut stop: watch::Receiver<()>,
) {
    while tokio::time::timeout(ADDRESS_BOOK_INTERVAL, stop.changed())
        .await
        .is_err()
    {
        let peers = book_peers(&identities, endpoint.id()).await;
        if let Err(err) = book.refresh(&endpoint, peers).await {
            tracing::warn!("the address book was not saved: {err:#}");
        }
    }
}

/// Every peer a hosted replica has synced with, as its store records them,
/// or names as a contact; never this node.
async fn book_peers(identities: &Identities, own: EndpointId) -> BTreeSet<EndpointId> {
    let stacks: Vec<Arc<HostedStack>> = match identities.read() {
        Ok(guard) => guard.values().cloned().collect(),
        Err(_poisoned) => return BTreeSet::new(),
    };
    let mut peers = BTreeSet::new();
    for stack in stacks {
        for tracked in stack.tracked_snapshot() {
            peers.extend(tracked.contacts.iter().map(|contact| contact.addr.id));
            if let Ok(Some(synced)) = tracked.doc.get_sync_peers().await {
                peers.extend(
                    synced
                        .iter()
                        .filter_map(|peer| EndpointId::from_bytes(peer).ok()),
                );
            }
        }
    }
    peers.remove(&own);
    peers
}

/// No timeout: the local socket's address appears as soon as any transport
/// address is published.
async fn wait_until_dialable(endpoint: &Endpoint) {
    while endpoint.watch_addr().get().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `Ok(None)` covers both "no such entry" and "record stored, payload not
/// yet fetched". Every payload-waiting read in this layer goes through here.
pub(crate) async fn read_payload(
    doc: &Doc,
    blobs: &iroh_blobs::api::Store,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let query = Query::single_latest_per_key().key_exact(key);
    let Some(entry) = doc.get_one(query).await? else {
        return Ok(None);
    };
    let hash = entry.content_hash();
    if !blobs.has(hash).await? {
        return Ok(None);
    }
    Ok(Some(blobs.get_bytes(hash).await?.to_vec()))
}

/// Every `interval`, re-request a sync for each hosted identity's tracked
/// docs but a pod's stores, which [`pod_reconcile_pass`] runs, with their
/// contacts (the engine unions in the peers it recorded), and reconcile
/// each co-located pair unless its write counts ([`PairReading`]) stand
/// where its last successful pass session found them. A failed request is
/// retried by the next pass. Ends when `stop` is sent or its sender is
/// dropped with the node.
async fn reconcile_pass(
    interval: Duration,
    identities: Identities,
    co_located_sessions: CoLocatedPassSessions,
    probes: Arc<PassProbes>,
    mut stop: watch::Receiver<()>,
) {
    let mut reconciled: HashMap<(PdnId, PdnId, NamespaceId), PairReading> = HashMap::new();
    while tokio::time::timeout(interval, stop.changed())
        .await
        .is_err()
    {
        let stacks: Vec<Arc<HostedStack>> = match identities.read() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(_poisoned) => continue,
        };
        for stack in &stacks {
            for tracked in stack.tracked_snapshot() {
                if matches!(stack.registry.pod_of(tracked.doc.id()), Ok(Some(_))) {
                    continue;
                }
                let _ = tracked
                    .doc
                    .start_sync(tracked.contacts, tracked.default_identity)
                    .await;
            }
        }
        reconcile_co_located(&stacks, &co_located_sessions, &mut reconciled).await;
        probes
            .passes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Every `interval`, reconcile each store of every pod a hosted identity
/// holds with at most [`POD_RECONCILE_PEERS`] peers, drawn afresh from its
/// contacts — derived first from the pod's membership — and the peers the
/// engine recorded. A failed run is retried by the next. Ends as
/// [`reconcile_pass`] does.
async fn pod_reconcile_pass(
    interval: Duration,
    node: EndpointId,
    identities: Identities,
    probes: Arc<PassProbes>,
    mut stop: watch::Receiver<()>,
) {
    while tokio::time::timeout(interval, stop.changed())
        .await
        .is_err()
    {
        let stacks: Vec<Arc<HostedStack>> = match identities.read() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(_poisoned) => continue,
        };
        for stack in &stacks {
            let Ok(pods) = stack.registry.pods() else {
                continue;
            };
            for (pod, held) in pods {
                reconcile_pod(stack, node, pod, &held, &probes).await;
            }
        }
    }
}

async fn reconcile_pod(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    probes: &PassProbes,
) {
    let derived = derive_pod_contacts(stack, node, pod, held, &[]).await;
    // A replica two runs in a row find holding nothing — its siblings gone
    // before it took the store — names this device itself; one run may fall
    // before a sibling's first session.
    let empty_twice = {
        let mut found = stack
            .found_empty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if derived.nobody {
            !found.insert(pod)
        } else {
            found.remove(&pod);
            false
        }
    };
    if empty_twice && held.records.is_some() {
        let identity = stack.identity;
        stack.report(PodNotice::Unlisted { identity, pod });
    }
    let stores = std::iter::once((PodStore::Membership, &held.membership)).chain(
        held.records
            .as_ref()
            .map(|records| (PodStore::Records, records)),
    );
    for (store, doc) in stores {
        let Ok(Some(tracked)) = stack.tracked(doc.id()) else {
            continue;
        };
        let drawn = draw(tracked.contacts.clone());
        probes.report(|| PodPassDraw {
            identity: stack.identity,
            pod,
            store,
            contacts: tracked.contacts.clone(),
            drawn: drawn.clone(),
        });
        let _ = sync_pod_store(stack, &tracked, drawn).await;
    }
}

/// Reconcile a pod store with `contacts`, in its swarm as `tracked` read.
/// A store `forget_pod` turned out of its swarm after that read leaves the
/// swarm again: the forget's own leave can run before this sync joins.
async fn sync_pod_store(
    stack: &HostedStack,
    tracked: &TrackedDoc,
    contacts: Vec<Contact>,
) -> Result<()> {
    let swarm = tracked.strategy == SyncStrategy::Swarm;
    start_pod_store(&tracked.doc, contacts, tracked.default_identity, swarm).await?;
    let turned_out = stack
        .tracked(tracked.doc.id())?
        .is_some_and(|now| now.strategy == SyncStrategy::ContactsOnly);
    if swarm && turned_out {
        tracked.doc.leave_gossip().await?;
    }
    Ok(())
}

/// Start, or run again, a pod store's sync with `contacts` alone: a peer
/// the engine recorded is a node the membership derivation may leave out —
/// a former member's device, or a member's a converged tombstone is done with.
async fn start_pod_store(
    doc: &Doc,
    contacts: Vec<Contact>,
    default_identity: Identity,
    join_gossip: bool,
) -> Result<()> {
    doc.sync_with_peers(contacts, Vec::new(), default_identity, join_gossip)
        .await
}

/// Derive both of a pod's stores' contacts from its membership and set
/// them whole: as the stores are tracked, and as the engine dials each
/// peer, since a peer a gossip message or a recorded session names is
/// dialed as what contacts stated for it.
/// What a derivation found of the identity's own standing.
#[derive(Default)]
struct Derived {
    /// The membership view shows the identity's own departure.
    departed: bool,
    /// The membership view lists nobody.
    nobody: bool,
}

async fn derive_pod_contacts(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[Identity],
) -> Derived {
    let derivation = stack.pod_derivation(pod);
    let _derivation = derivation.lock().await;
    derive_pod_contacts_locked(stack, node, pod, held, met).await
}

/// [`derive_pod_contacts`] with the pod's derivation lock held.
async fn derive_pod_contacts_locked(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[Identity],
) -> Derived {
    let derived = match pod_contacts(stack, node, pod, held, met).await {
        Ok(derived) => derived,
        Err(err) => {
            let identity = stack.identity;
            tracing::warn!(%identity, %pod, "deriving the pod's contacts failed: {err:#}");
            return Derived::default();
        }
    };
    let found = Derived {
        departed: derived.departed.is_some(),
        nobody: derived.contacts.is_none(),
    };
    let identity = stack.identity;
    if held.records.is_some() {
        if let Some(seq) = derived.departed {
            stack.report(PodNotice::Departed { identity, pod, seq });
        }
        if derived.unlisted {
            stack.report(PodNotice::Unlisted { identity, pod });
        }
    } else if derived.member {
        stack.report(PodNotice::Reinstated { identity, pod });
    }
    let Some(contacts) = derived.contacts else {
        return found;
    };
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        let _ = stack.set_contacts(doc.id(), contacts.clone());
        let _ = stack
            .docs
            .engine()
            .state_contacts(doc.id(), contacts.clone())
            .await;
    }
    found
}

/// Derive `held`'s contacts before its sync starts, so a reopened replica
/// dials each member's devices as that member — a ticket names every node
/// as the identity that minted it — and keep beside them each ticket
/// contact whose node they leave out: a replica whose membership view lists
/// nobody yet, or a tombstone past its convergence, has no other.
async fn derive_before_start(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
) -> Result<()> {
    let derivation = stack.pod_derivation(pod);
    let _derivation = derivation.lock().await;
    let docs: Vec<&Doc> = std::iter::once(&held.membership)
        .chain(held.records.as_ref())
        .collect();
    let mut ticketed = Vec::new();
    for doc in &docs {
        ticketed.push(
            stack
                .tracked(doc.id())?
                .map(|tracked| tracked.contacts)
                .unwrap_or_default(),
        );
    }
    derive_pod_contacts_locked(stack, node, pod, held, &[]).await;
    for (doc, ticketed) in docs.into_iter().zip(ticketed) {
        let Some(tracked) = stack.tracked(doc.id())? else {
            continue;
        };
        let mut contacts = tracked.contacts;
        for contact in ticketed {
            if !contacts.iter().any(|kept| kept.addr.id == contact.addr.id) {
                contacts.push(contact);
            }
        }
        stack.set_contacts(doc.id(), contacts.clone())?;
        stack
            .docs
            .engine()
            .state_contacts(doc.id(), contacts)
            .await?;
    }
    Ok(())
}

/// Dial every store of the identity's pods toward each device its
/// PMS newly lists, as the identity: a sibling whose first dial came
/// before its listing reached this device is refused, and nothing else
/// dials it again before the pod stores' pass. Ends with the PMS's
/// subscription or the identity's half of the node.
fn watch_own_devices(stack: &Arc<HostedStack>, node: EndpointId, pms: Doc) {
    let stack = Arc::downgrade(stack);
    let _detached = tokio::spawn(async move {
        let Ok(events) = pms.subscribe().await else {
            return;
        };
        // A burst can end with the stream, which panics when polled again.
        let mut events = events.fuse();
        let mut listed: HashSet<NodeId> = crate::private_metadata::listed_devices(&pms)
            .await
            .map(|devices| devices.into_iter().collect())
            .unwrap_or_default();
        while events.next().await.is_some() {
            while let Some(Some(_queued)) = futures_lite::future::poll_once(events.next()).await {}
            let Ok(now) = crate::private_metadata::listed_devices(&pms).await else {
                continue;
            };
            let fresh: Vec<NodeId> = now
                .iter()
                .filter(|device| !listed.contains(*device))
                .copied()
                .collect();
            listed.extend(now);
            if fresh.is_empty() {
                continue;
            }
            let Some(stack) = stack.upgrade() else {
                return;
            };
            dial_own_devices(&stack, node, &fresh).await;
        }
    });
}

async fn dial_own_devices(stack: &HostedStack, node: EndpointId, devices: &[NodeId]) {
    let own = stack.identity();
    let contacts: Vec<Contact> = devices
        .iter()
        .filter_map(|device| EndpointId::from_bytes(device.as_bytes()).ok())
        .filter(|device| *device != node)
        .map(|device| Contact::new(EndpointAddr::new(device), own))
        .collect();
    if contacts.is_empty() {
        return;
    }
    let Ok(pods) = stack.registry.pods() else {
        return;
    };
    for (pod, held) in pods {
        derive_pod_contacts(stack, node, pod, &held, &[]).await;
        for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
            let Ok(Some(tracked)) = stack.tracked(doc.id()) else {
                continue;
            };
            if let Err(err) = sync_pod_store(stack, &tracked, contacts.clone()).await {
                tracing::warn!(%pod, "dialing a newly listed device failed: {err:#}");
            }
        }
    }
}

/// Derive `pod`'s contacts again whenever its membership store changes —
/// at once when an entry or a payload arrives, since either may list a
/// device, and `settle` after anything else — so a newcomer is dialed as
/// the member it is from its first announcement on; a local write derives
/// in the write itself. While the identity's own departure stands, a
/// session that brought entries is dialed again at once: what such a
/// replica takes ([`Rests`](crate::pod::Rests)) reaches one layer further
/// with each entry it holds; once it ends with the record store held, both
/// stores are reconciled at once. Ends with the store's subscription or the
/// identity's half of the node.
fn watch_pod_membership(
    stack: &Arc<HostedStack>,
    node: EndpointId,
    pod: PodId,
    membership: &Doc,
    settle: Duration,
) {
    let stack = Arc::downgrade(stack);
    let membership = membership.clone();
    let _detached = tokio::spawn(async move {
        let Ok(events) = membership.subscribe().await else {
            return;
        };
        // A burst can end with the stream, which panics when polled again.
        let mut events = events.fuse();
        let mut seen = Seen::default();
        let mut departed = false;
        while let Some(event) = events.next().await {
            // What is already queued joins this derivation; what arrives
            // during it derives again after it, so a store that never goes
            // quiet still derives.
            seen.met.clear();
            let mut listing = seen.note(event);
            while let Some(Some(event)) = futures_lite::future::poll_once(events.next()).await {
                listing |= seen.note(event);
            }
            if !listing {
                let settled = tokio::time::Instant::now() + settle;
                while let Ok(Some(event)) = tokio::time::timeout_at(settled, events.next()).await {
                    if seen.note(event) {
                        break;
                    }
                }
            }
            let Some(stack) = stack.upgrade() else {
                return;
            };
            let Ok(Some(held)) = stack.registry.pod(pod) else {
                continue;
            };
            let met: Vec<Identity> = seen.met.iter().map(|(_peer, identity)| *identity).collect();
            let derived = derive_pod_contacts(&stack, node, pod, &held, &met).await;
            let again = seen.brought();
            if derived.departed && !again.is_empty() {
                if let Err(err) = dial_again(&stack, &held, again).await {
                    tracing::warn!(%pod, "dialing a departed replica's counterpart again failed: {err:#}");
                }
            }
            // The record store's sessions while the departure stood took
            // nothing, and none follows the change that ended it.
            if departed && !derived.departed && held.records.is_some() {
                if let Err(err) = dial_drawn(&stack, &held).await {
                    tracing::warn!(%pod, "reconciling a pod held again failed: {err:#}");
                }
            }
            departed = derived.departed;
        }
    });
}

/// What a membership store's watcher has seen of its sessions.
#[derive(Default)]
struct Seen {
    /// The peer and counterpart of each session that went through since the
    /// last derivation.
    met: Vec<(EndpointId, Identity)>,
    /// The peers an entry arrived from since their last session ended.
    brought_by: HashSet<EndpointId>,
}

impl Seen {
    /// Whether the event may list a device the contacts lack — an entry or
    /// a payload arriving, or events dropped unread.
    fn note(&mut self, event: Result<pdn_store::engine::LiveEvent>) -> bool {
        use pdn_store::engine::LiveEvent;
        match event {
            Ok(LiveEvent::SyncFinished(sync)) => {
                if sync.result.is_ok() {
                    self.met.push((sync.peer, sync.counterpart));
                }
                false
            }
            Ok(LiveEvent::InsertRemote { from, .. }) => {
                self.brought_by.insert(from);
                true
            }
            Ok(
                LiveEvent::InsertLocal { .. }
                | LiveEvent::PendingContentReady
                | LiveEvent::NeighborUp(_)
                | LiveEvent::NeighborDown(_),
            ) => false,
            Ok(LiveEvent::ContentReady { .. } | LiveEvent::Lagged) | Err(_) => true,
        }
    }

    /// The sessions met since the last derivation that brought an entry,
    /// each peer's mark cleared.
    fn brought(&mut self) -> Vec<(EndpointId, Identity)> {
        let mut brought = Vec::new();
        for (peer, identity) in &self.met {
            if self.brought_by.remove(peer) {
                brought.push((*peer, *identity));
            }
        }
        brought
    }
}

/// Reconcile the stores `held` names with at most [`POD_RECONCILE_PEERS`]
/// of the membership store's contacts, drawn afresh, the membership
/// store's exchange first.
async fn dial_drawn(stack: &HostedStack, held: &PodBinding) -> Result<()> {
    let Some(tracked) = stack.tracked(held.membership.id())? else {
        return Ok(());
    };
    let drawn = draw(tracked.contacts);
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        if let Some(tracked) = stack.tracked(doc.id())? {
            sync_pod_store(stack, &tracked, drawn.clone()).await?;
        }
    }
    Ok(())
}

/// Dial each of `sessions` again on the stores `held` names, the
/// membership store's exchange first, each peer at the address its
/// contacts carry.
async fn dial_again(
    stack: &HostedStack,
    held: &PodBinding,
    sessions: Vec<(EndpointId, Identity)>,
) -> Result<()> {
    let known = known_addresses(stack, &held.membership)?;
    let contacts: Vec<Contact> = sessions
        .into_iter()
        .map(|(peer, identity)| {
            let addr = known
                .get(&peer)
                .cloned()
                .unwrap_or_else(|| EndpointAddr::new(peer));
            Contact::new(addr, identity)
        })
        .collect();
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        if let Some(tracked) = stack.tracked(doc.id())? {
            sync_pod_store(stack, &tracked, contacts.clone()).await?;
        }
    }
    Ok(())
}

/// A pod store's contacts: every device the statements of the pod's
/// current members list, each dialed as its member, and the identity's own
/// devices by its PMS, dialed as the identity; never this device as
/// this identity. `None` while the membership view lists nobody: a replica
/// that holds nothing yet keeps the contacts its ticket gave it.
/// A tombstone drops the members' devices once a session with a current
/// member's replica — a counterpart among `met`, on whichever node — went
/// through. A
/// replica holding its record store again keeps them while its membership
/// view still shows the departure: they are how a rejoin dials its inviter.
async fn pod_contacts(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[Identity],
) -> Result<DerivedContacts> {
    let membership = &held.membership;
    let view = stack.access.membership_view(pod, membership).await?;
    if view.identities().next().is_none() {
        return Ok(DerivedContacts {
            contacts: None,
            departed: None,
            member: false,
            unlisted: false,
        });
    }
    let standing = view.member(&stack.identity);
    let departed = standing.and_then(Member::departed_at).map(Seq::new);
    let member = standing.is_some_and(|standing| standing.state.member);
    let own = stack.identity();
    let own_devices = stack.access.own_devices().await?;
    let this_device = NodeId::from_bytes(*node.as_bytes());
    let device = MemberDevice {
        node: this_device,
        author: stack.author,
    };
    let unlisted = member && standing.is_some_and(|standing| !standing.devices.contains(&device));
    let converged = {
        let mut converged = stack
            .converged_tombstones
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("tombstone lock poisoned"))?;
        if departed.is_some() && held.records.is_none() {
            let member = |counterpart: &Identity| {
                view.member(&PdnId::from_bytes(*counterpart.as_bytes()))
                    .is_some_and(|member| member.state.member)
            };
            if met.iter().any(member) {
                converged.insert(pod);
            }
        } else {
            converged.remove(&pod);
        }
        converged.contains(&pod)
    };
    let mut devices: Vec<(NodeId, Identity)> = view
        .identities()
        .filter(|(_id, member)| member.state.member && !converged)
        .flat_map(|(id, member)| {
            let identity = crate::access::identity_of(*id);
            member
                .devices
                .iter()
                .map(move |device| (device.node, identity))
        })
        .collect();
    devices.extend(own_devices.into_iter().map(|device| (device, own)));
    if !converged {
        devices.extend(ticketed_unlisted(stack, held, &view)?);
    }
    let known = known_addresses(stack, membership)?;
    let mut seen = HashSet::new();
    let mut contacts = Vec::new();
    for (device, identity) in devices {
        let Ok(id) = EndpointId::from_bytes(device.as_bytes()) else {
            continue;
        };
        if (id == node && identity == own) || !seen.insert((id, identity)) {
            continue;
        }
        let addr = known
            .get(&id)
            .cloned()
            .unwrap_or_else(|| EndpointAddr::new(id));
        contacts.push(Contact::new(addr, identity));
    }
    Ok(DerivedContacts {
        contacts: Some(contacts),
        departed,
        member,
        unlisted,
    })
}

/// The devices `held`'s tickets name as a current member that no statement
/// lists yet: the inviter's other device, reached before its statement is.
fn ticketed_unlisted(
    stack: &HostedStack,
    held: &PodBinding,
    view: &MembershipView,
) -> Result<Vec<(NodeId, Identity)>> {
    let listed: HashSet<NodeId> = view
        .identities()
        .flat_map(|(_id, member)| member.devices.iter().map(|device| device.node))
        .collect();
    let mut devices = Vec::new();
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        for contact in stack
            .tracked(doc.id())?
            .map(|tracked| tracked.ticketed)
            .unwrap_or_default()
        {
            let device = NodeId::from_bytes(*contact.addr.id.as_bytes());
            let current = view
                .member(&PdnId::from_bytes(*contact.identity.as_bytes()))
                .is_some_and(|member| member.state.member);
            if current && !listed.contains(&device) {
                devices.push((device, contact.identity));
            }
        }
    }
    Ok(devices)
}

/// The contacts of `ticket` and of `others`, tickets to the same store,
/// each node named as the identity whose ticket lists it.
fn contacts_of<'a>(
    ticket: &DocTicket,
    others: impl Iterator<Item = &'a DocTicket>,
) -> Result<Vec<Contact>> {
    let namespace = ticket.capability.id();
    let mut contacts = ticket.contacts();
    for other in others {
        anyhow::ensure!(
            other.capability.id() == namespace,
            "a ticket to {} is no ticket to {namespace}",
            other.capability.id()
        );
        for contact in other.contacts() {
            if !contacts
                .iter()
                .any(|known| known.addr.id == contact.addr.id && known.identity == contact.identity)
            {
                contacts.push(contact);
            }
        }
    }
    Ok(contacts)
}

/// The addresses a store's contacts carry, a ticket's among them: without
/// address lookup a node id alone is undialable to a device that never met
/// it, a restarted one included.
fn known_addresses(stack: &HostedStack, store: &Doc) -> Result<HashMap<EndpointId, EndpointAddr>> {
    Ok(stack
        .tracked(store.id())?
        .map(|tracked| {
            tracked
                .ticketed
                .into_iter()
                .chain(tracked.contacts)
                .map(|contact| (contact.addr.id, contact.addr))
                .collect()
        })
        .unwrap_or_default())
}

/// What [`pod_contacts`] derives from one membership view.
struct DerivedContacts {
    /// `None` while the membership view lists nobody.
    contacts: Option<Vec<Contact>>,
    /// The sequence of the identity's own departure.
    departed: Option<Seq>,
    /// The identity is a member.
    member: bool,
    /// The identity is a member, and no statement of its own lists this
    /// device with the author it writes with here.
    unlisted: bool,
}

/// At most [`POD_RECONCILE_PEERS`] of a store's contacts, drawn afresh.
fn draw(mut contacts: Vec<Contact>) -> Vec<Contact> {
    contacts.shuffle(&mut rand::rng());
    contacts.truncate(POD_RECONCILE_PEERS);
    contacts
}

/// The sessions a [`SyncNode::flush_pod`] dialed, as each store finishes
/// them.
pub struct PodFlush {
    /// A session started before it may have missed the latest writes. A
    /// clock stepping back past it leaves the wait to its bound.
    since: std::time::SystemTime,
    peers: HashMap<NodeId, EndpointAddr>,
    sessions: Vec<
        std::pin::Pin<
            Box<dyn futures_core::Stream<Item = Result<pdn_store::engine::LiveEvent>> + Send>,
        >,
    >,
    /// By hash, with the length its entry states, as the flush began.
    payloads: Vec<(Hash, u64)>,
    endpoint: Endpoint,
    blobs: iroh_blobs::api::Store,
}

impl std::fmt::Debug for PodFlush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PodFlush")
            .field("peers", &self.peers.keys())
            .finish_non_exhaustive()
    }
}

impl PodFlush {
    /// Whether a session with one of the dialed devices went through on
    /// each store within `timeout`, and the device whose session ended the
    /// last store came to hold every payload of the identity's own entries,
    /// within `timeout` and what the missing bytes take at
    /// [`FLUSH_PAYLOAD_RATE`]; `false` at once when none was dialed.
    pub async fn wait(self, timeout: Duration) -> bool {
        let PodFlush {
            since,
            peers,
            sessions,
            payloads,
            endpoint,
            blobs,
        } = self;
        if peers.is_empty() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut served_by = None;
        for mut events in sessions {
            loop {
                match tokio::time::timeout_at(deadline, events.next()).await {
                    Ok(Some(Ok(pdn_store::engine::LiveEvent::SyncFinished(sync))))
                        if sync.result.is_ok() && sync.started >= since =>
                    {
                        let peer = NodeId::from_bytes(*sync.peer.as_bytes());
                        if peers.contains_key(&peer) {
                            served_by = Some(peer);
                            break;
                        }
                    }
                    Ok(Some(_other)) => {}
                    Ok(None) | Err(_) => return false,
                }
            }
        }
        let Some(peer) = served_by else {
            return true;
        };
        let held = FlushedPayloads {
            payloads,
            endpoint,
            blobs,
        };
        match peers.get(&peer) {
            Some(addr) => held.held_by(addr, deadline).await,
            None => false,
        }
    }
}

/// What [`PodFlush::wait`] asks the device that took the flush about once
/// its sessions went through; apart from the sessions, which are not
/// `Sync`, so a wait over it stays `Send`.
struct FlushedPayloads {
    payloads: Vec<(Hash, u64)>,
    endpoint: Endpoint,
    blobs: iroh_blobs::api::Store,
}

impl FlushedPayloads {
    /// The first answer for every payload within `deadline` says which the
    /// device lacks; their bytes at [`FLUSH_PAYLOAD_RATE`] then bound the
    /// wait for the rest. A device on this node shares its blob store.
    async fn held_by(&self, addr: &EndpointAddr, deadline: tokio::time::Instant) -> bool {
        if self.payloads.is_empty() || addr.id == self.endpoint.id() {
            return true;
        }
        let Ok(Ok(conn)) =
            tokio::time::timeout_at(deadline, self.endpoint.connect(addr.clone(), BLOBS_ALPN))
                .await
        else {
            return false;
        };
        let held_now = self.payloads.iter().copied().map(|(hash, len)| {
            let mut updates = Box::pin(self.blobs.remote().observe(
                conn.clone(),
                iroh_blobs::protocol::ObserveRequest::new(hash),
            ));
            async move {
                let complete =
                    matches!(updates.next().await, Some(Ok(first)) if first.is_complete());
                (hash, len, complete)
            }
        });
        let Some(firsts) = bounded(held_now, deadline).await else {
            return false;
        };
        let missing: Vec<(Hash, u64)> = firsts
            .into_iter()
            .filter(|(_hash, _len, complete)| !complete)
            .map(|(hash, len, _complete)| (hash, len))
            .collect();
        if missing.is_empty() {
            return true;
        }
        let missing_bytes = missing.iter().map(|(_hash, len)| *len).sum::<u64>();
        let transfer = Duration::from_secs(missing_bytes.div_ceil(FLUSH_PAYLOAD_RATE));
        let deadline = deadline.max(tokio::time::Instant::now() + transfer);
        let completions = missing.into_iter().map(|(hash, _len)| {
            let mut updates = Box::pin(self.blobs.remote().observe(
                conn.clone(),
                iroh_blobs::protocol::ObserveRequest::new(hash),
            ));
            async move {
                let mut held = iroh_blobs::api::proto::Bitfield::empty();
                while let Some(Ok(update)) = updates.next().await {
                    held.update(&update);
                    if held.is_complete() {
                        return true;
                    }
                }
                false
            }
        });
        bounded(completions, deadline)
            .await
            .is_some_and(|completed| completed.into_iter().all(|complete| complete))
    }
}

/// Every one of `tasks`, at most [`FLUSH_OBSERVES`] at a time, each opening
/// its stream only as it starts; `None` past `deadline` or when a task
/// fails.
async fn bounded<T, F>(
    tasks: impl Iterator<Item = F>,
    deadline: tokio::time::Instant,
) -> Option<Vec<T>>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let run = async {
        let mut tasks = tasks.peekable();
        let mut running = tokio::task::JoinSet::new();
        let mut done = Vec::new();
        loop {
            while running.len() < FLUSH_OBSERVES {
                let Some(task) = tasks.next() else {
                    break;
                };
                running.spawn(task);
            }
            match running.join_next().await {
                Some(Ok(result)) => done.push(result),
                Some(Err(_panicked)) => return None,
                None => return Some(done),
            }
        }
    };
    tokio::time::timeout_at(deadline, run).await.ok().flatten()
}

/// Every payload the entries of `identity`'s devices in `pod`'s stores
/// reference, by its membership view's statements and this device's own
/// author: the ones a departure leaves no copy of on those devices.
async fn own_payloads(
    stack: &HostedStack,
    pod: PodId,
    held: &PodBinding,
) -> Result<Vec<(Hash, u64)>> {
    let view = stack.access.membership_view(pod, &held.membership).await?;
    let authors: HashSet<AuthorId> = view
        .member(&stack.identity)
        .into_iter()
        .flat_map(|member| member.devices.iter().map(|device| device.author))
        .chain(std::iter::once(stack.author))
        .collect();
    let mut payloads = HashMap::new();
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        let mut entries = std::pin::pin!(doc.get_many(Query::all()).await?);
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if entry.content_len() > 0 && authors.contains(&entry.author()) {
                payloads.insert(entry.content_hash(), entry.content_len());
            }
        }
    }
    Ok(payloads.into_iter().collect())
}

/// The entries `author` wrote into `doc` whose payload `blobs` does not hold.
async fn entries_without_payload(
    doc: &Doc,
    author: AuthorId,
    blobs: &iroh_blobs::api::Store,
) -> Result<usize> {
    let mut missing = 0usize;
    let mut entries = std::pin::pin!(doc.get_many(Query::all()).await?);
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        if entry.author() == author
            && entry.content_len() > 0
            && !blobs.has(entry.content_hash()).await?
        {
            missing = missing.saturating_add(1);
        }
    }
    Ok(missing)
}

/// The hold [`SyncNode::pause_next_pod_start_for_test`] puts on a pod
/// start: `reached` once its membership store's sync has started.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub struct PodStartPause {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

/// The hold [`SyncNode::pause_next_collection_for_test`] puts on a blob
/// collection run: `reached` once the run has read every hosted identity's
/// replicas.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub struct CollectionPause {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(feature = "test-util")]
type CollectionPauseSlot = Arc<std::sync::Mutex<Option<Arc<CollectionPause>>>>;

/// The hold [`SyncNode::hold_next_blob_stream_for_test`] puts on a blob
/// stream: `reached` once it has read its request, `done` once it has
/// answered it.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub struct BlobStreamHold {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub done: tokio::sync::Notify,
}

#[cfg(feature = "test-util")]
type BlobStreamHoldSlot = Arc<std::sync::Mutex<Option<Arc<BlobStreamHold>>>>;

/// One step of a node's stop, as [`SyncNode::stop_steps_for_test`] lists
/// them.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStep {
    Engines,
    Router,
    BlobStreams,
    BlobStore,
}

#[cfg(feature = "test-util")]
pub type StopSteps = Arc<std::sync::Mutex<Vec<StopStep>>>;

/// The sessions one replica of a pod's store finishes, from
/// [`SyncNode::watch_pod_sessions`]. Unread past its buffer it drops
/// events.
#[cfg(feature = "test-util")]
pub struct PodSessions {
    events: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<pdn_store::engine::LiveEvent>> + Send>,
    >,
}

#[cfg(feature = "test-util")]
impl std::fmt::Debug for PodSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PodSessions").finish_non_exhaustive()
    }
}

/// One session a pod store's replica finished.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone)]
pub struct PodSession {
    pub peer: NodeId,
    pub dialed: bool,
    pub started: std::time::SystemTime,
    /// The session's entries received and sent, or why it failed or was
    /// refused.
    pub exchanged: std::result::Result<(usize, usize), String>,
}

#[cfg(feature = "test-util")]
impl PodSessions {
    /// The next session with `peer` that went through, whichever side
    /// dialed, or `None` once `timeout` passes first.
    pub async fn next_served_with(
        &mut self,
        peer: NodeId,
        timeout: Duration,
    ) -> Result<Option<PodSession>> {
        self.next_matching(timeout, |session| {
            session.peer == peer && session.exchanged.is_ok()
        })
        .await
    }

    /// The next session with `peer` started at `since` or later, whichever
    /// side dialed and however it ended, or `None` once `timeout` passes
    /// first.
    pub async fn next_started_with(
        &mut self,
        peer: NodeId,
        since: std::time::SystemTime,
        timeout: Duration,
    ) -> Result<Option<PodSession>> {
        self.next_matching(timeout, |session| {
            session.peer == peer && session.started >= since
        })
        .await
    }

    /// The next session finished with `peer` that this replica `dialed`,
    /// or accepted, or `None` once `timeout` passes first.
    pub async fn next_with(
        &mut self,
        peer: NodeId,
        dialed: bool,
        timeout: Duration,
    ) -> Result<Option<PodSession>> {
        self.next_matching(timeout, |session| {
            session.peer == peer && session.dialed == dialed
        })
        .await
    }

    async fn next_matching(
        &mut self,
        timeout: Duration,
        wanted: impl Fn(&PodSession) -> bool,
    ) -> Result<Option<PodSession>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Ok(event) = tokio::time::timeout_at(deadline, self.events.next()).await else {
                return Ok(None);
            };
            let event = event.context("the replica's event stream ended")??;
            let pdn_store::engine::LiveEvent::SyncFinished(sync) = event else {
                continue;
            };
            let session = PodSession {
                peer: NodeId::from_bytes(*sync.peer.as_bytes()),
                dialed: matches!(sync.origin, pdn_store::engine::Origin::Connect(_)),
                started: sync.started,
                exchanged: sync
                    .result
                    .map(|details| (details.entries_received, details.entries_sent)),
            };
            if wanted(&session) {
                return Ok(Some(session));
            }
        }
    }
}

/// Whom one run of the pod stores' pass reached for one store.
#[derive(Debug, Clone)]
pub struct PodPassDraw {
    pub identity: PdnId,
    pub pod: PodId,
    pub store: PodStore,
    /// The contacts the store was tracked with when the run drew.
    pub contacts: Vec<Contact>,
    pub drawn: Vec<Contact>,
}

/// What the passes report: how many runs of [`reconcile_pass`] finished,
/// and, once a scenario takes the channel, every draw of
/// [`pod_reconcile_pass`].
#[derive(Debug, Default)]
struct PassProbes {
    passes: std::sync::atomic::AtomicU64,
    draws: Mutex<Option<tokio::sync::mpsc::UnboundedSender<PodPassDraw>>>,
}

impl PassProbes {
    fn report(&self, draw: impl FnOnce() -> PodPassDraw) {
        if let Ok(sender) = self.draws.lock() {
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(draw());
            }
        }
    }
}

/// Sessions the pass has opened, per namespace: a pair holds a pod's two
/// stores, and a scenario reads each apart.
type CoLocatedPassSessions = Arc<Mutex<HashMap<NamespaceId, u64>>>;

/// What a co-located pair looked like before its last successful pass
/// session: how many writes each side's replica had taken, both sides in
/// the pair's canonical order. A pass skips the pair only while every count
/// stands still.
#[derive(PartialEq, Eq, Clone, Copy)]
struct PairReading {
    source_writes: u64,
    target_writes: u64,
    /// A pod's record store beside its membership store, the two read as
    /// one pair.
    source_records: u64,
    target_records: u64,
}

/// The pair in a fixed order, so the memo reads the same whichever side
/// the pass happened to walk from: the stacks come from a hash map, and a
/// key that depended on that order would miss itself on a rehash and
/// reconcile a quiet pair.
fn ordered<'a>(
    source: &'a Arc<HostedStack>,
    target: &'a Arc<HostedStack>,
) -> (&'a Arc<HostedStack>, &'a Arc<HostedStack>) {
    if source.identity <= target.identity {
        (source, target)
    } else {
        (target, source)
    }
}

async fn pair_reading(
    source: &Arc<HostedStack>,
    target: &Arc<HostedStack>,
    namespace: NamespaceId,
) -> Option<PairReading> {
    let (first, second) = ordered(source, target);
    let (source_records, target_records) = match pod_records(first, namespace) {
        Some(records) => (
            first.docs.engine().sync.writes(records).await.ok()?,
            second.docs.engine().sync.writes(records).await.ok()?,
        ),
        None => (0, 0),
    };
    Some(PairReading {
        source_writes: first.docs.engine().sync.writes(namespace).await.ok()?,
        target_writes: second.docs.engine().sync.writes(namespace).await.ok()?,
        source_records,
        target_records,
    })
}

/// The record store of the pod whose membership store `namespace` is.
fn pod_records(stack: &HostedStack, namespace: NamespaceId) -> Option<NamespaceId> {
    let Ok(Some((pod, PodStore::Membership))) = stack.registry.pod_of(namespace) else {
        return None;
    };
    Some(stack.registry.pod(pod).ok()??.records?.id())
}

/// Reconcile each pair of hosted identities holding one namespace, and
/// leave alone a pair where nothing has moved since the pass last
/// reconciled it — a pass over a quiet namespace must not accumulate
/// sessions.
async fn reconcile_co_located(
    stacks: &[Arc<HostedStack>],
    opened: &CoLocatedPassSessions,
    reconciled: &mut HashMap<(PdnId, PdnId, NamespaceId), PairReading>,
) {
    for (index, source) in stacks.iter().enumerate() {
        for target in stacks.iter().skip(index + 1) {
            for tracked in source.tracked_snapshot() {
                let namespace = tracked.doc.id();
                if target.tracked(namespace).ok().flatten().is_none() {
                    continue;
                }
                // Reconciled after its membership store, the two one pair.
                if governing_store(source, namespace).is_some() {
                    continue;
                }
                let (first, second) = ordered(source, target);
                let pair = (first.identity, second.identity, namespace);
                let Some(reading) = pair_reading(source, target, namespace).await else {
                    continue;
                };
                if reconciled.get(&pair) == Some(&reading) {
                    continue;
                }
                let records = pod_records(source, namespace)
                    .filter(|records| matches!(target.tracked(*records), Ok(Some(_))));
                if let Ok(mut opened) = opened.lock() {
                    for reconciled in std::iter::once(namespace).chain(records) {
                        *opened.entry(reconciled).or_default() += 1;
                    }
                }
                let mut reconciliation = source
                    .docs
                    .engine()
                    .sync_in_process(target.docs.engine(), namespace, target.identity())
                    .await;
                if let (Ok(()), Some(records)) = (&reconciliation, records) {
                    reconciliation = source
                        .docs
                        .engine()
                        .sync_in_process(target.docs.engine(), records, target.identity())
                        .await;
                }
                // The reading taken before the session is what is kept, and
                // only if the session went through. Reading again after it
                // would race the ingest — the receiving side applies what
                // arrived in its own actor, which the dialing half does not
                // wait for — and a reading taken too early would differ on
                // the next pass, which would reconcile again, and again.
                // Kept this way, a session that delivered something costs
                // one more pass before the pair goes quiet, and a session
                // that failed leaves the pair as it was.
                if reconciliation.is_ok() {
                    reconciled.insert(pair, reading);
                }
            }
        }
    }
    reconciled.retain(|(source, target, namespace), _reading| {
        stacks.iter().any(|stack| {
            stack.identity == *source && matches!(stack.tracked(*namespace), Ok(Some(_)))
        }) && stacks.iter().any(|stack| stack.identity == *target)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each draw reaches at most five of a store's contacts, and draws
    /// differ from run to run until every contact has been reached.
    #[test]
    fn a_draw_reaches_at_most_five_contacts_afresh_each_run() {
        let key = |seed: u8| SecretKey::from_bytes(&[seed; 32]).public();
        let contacts: Vec<Contact> = (1..=8)
            .map(|seed| {
                Contact::new(
                    EndpointAddr::new(key(seed)),
                    Identity::from_bytes([seed; 32]),
                )
            })
            .collect();
        let mut reached = HashSet::new();
        for _run in 0..200 {
            let drawn = draw(contacts.clone());
            assert_eq!(drawn.len(), POD_RECONCILE_PEERS);
            reached.extend(drawn.iter().map(|contact| *contact.addr.id.as_bytes()));
        }
        assert_eq!(reached.len(), 8, "a contact went undrawn for 200 runs");
    }

    /// Tested directly: the directory's advisory lock refuses a second node
    /// before any store opens, so no scenario reaches this backstop, and a
    /// backstop nobody reaches rots when the fork's error shape drifts.
    #[test]
    fn a_held_database_is_reclassified_and_nothing_else_is() {
        let directory = std::path::PathBuf::from("/pdn/state");
        let storage = StorageConfig::Directory(directory.clone());

        // Wrapped in a context layer, the way the fork's chain presents it.
        let held = anyhow::Error::new(redb::DatabaseError::DatabaseAlreadyOpen)
            .context("cannot open the replica store");
        let annotated = annotate_store_error(held, &storage);
        let named = annotated
            .downcast_ref::<DirectoryHeld>()
            .expect("a database already open must be reclassified as a held directory");
        assert_eq!(named.directory, directory);

        let unrelated = anyhow::anyhow!("the store's file is corrupt");
        let annotated = annotate_store_error(unrelated, &storage);
        assert!(
            annotated.downcast_ref::<DirectoryHeld>().is_none(),
            "an unrelated store failure must not read as a held directory"
        );
        assert!(
            format!("{annotated:#}").contains(&directory.display().to_string()),
            "an unrelated store failure must still name the directory"
        );

        let on_memory = annotate_store_error(anyhow::anyhow!("boom"), &StorageConfig::Memory);
        assert_eq!(format!("{on_memory:#}"), "boom");
    }

    /// A stopped blob store fails the storage check while the replica store
    /// still answers, as a placement on a full disk leaves the two.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_blob_store_fails_the_storage_check() -> Result<()> {
        let node = SyncNode::spawn(SpawnOptions::memory()).await?;
        let alice = PdnId::from_bytes([0xa1; 32]);
        node.provision_identity(alice).await?;
        let _pms = crate::PrivateMetadataStore::create(&node, alice).await?;
        node.check_stores().await?;

        node.blobs().shutdown().await?;
        assert!(
            node.check_stores().await.is_err(),
            "a stopped blob store must fail the storage check"
        );
        for stack in node.stacks()? {
            if let Some(tracked) = stack.tracked_snapshot().first() {
                let _peers = tracked.doc.get_sync_peers().await?;
            }
        }

        node.shutdown().await?;
        Ok(())
    }
}
