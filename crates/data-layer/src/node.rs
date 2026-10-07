//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. Externally supplied protocols — pdn-node's pairing and
//! linking dialogues (ADR-0011, ADR-0012) — register on the same endpoint at
//! spawn; a narrow dial handle serves their dial sides. The registration
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
    endpoint::{default_relay_mode, presets, Connection},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, Watcher as _,
};
use iroh_blobs::{
    store::{fs::FsStore, mem::MemStore, GcConfig, ProtectCb, ProtectOutcome},
    BlobsProtocol, Hash, ALPN as BLOBS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc, DocsApi,
    },
    protocol::{Docs, DocsDispatch},
    store::Query,
    AuthorId, Contact, DocTicket, Identity, NamespaceId, PeerIdBytes, ALPN as DOCS_ALPN,
};
use pdn_types::{EntryInfo, EntryPath, NodeId, PdnId, PodId, RecordRef};
use rand::seq::SliceRandom as _;
use tokio::sync::watch;

use crate::{
    access::{capability_ingest_validator, session_access_provider, AccessBook},
    address_book::{AddressBook, ADDRESS_BOOK_INTERVAL},
    connection_metadata::ConnectionMetadataStore,
    pod::{
        departure_past, record_entries, record_prefix, unknown_entries, Member, MemberDevice,
        Membership, Operation, PodCatchUp, PodNotice, PodStore, PodTickets, RecordView, Seq,
        UnknownEntry, UnknownPod,
    },
    private_metadata::PrivateMetadataStore,
    registry::{PodBinding, Registry, ServingPosture},
    retraction::{RetractionTracker, RetractionVerdict},
};

/// `issuer` has no created or imported data namespace here. Downcast from
/// the `anyhow::Error` of the issuer-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("data namespace not bound on this node: {issuer}")]
pub struct UnknownIssuer {
    pub issuer: PdnId,
}

/// `identity` holds `issuer`'s namespace as a grantee and mints no ticket
/// on it: the store's share would rejoin the swarm and dial recorded peers
/// as the identity. Downcast from the `anyhow::Error` of
/// [`SyncNode::share_ticket`].
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "identity {identity} holds the data namespace of {issuer} as a grantee and cannot share it"
)]
pub struct GranteeCannotShare {
    pub identity: PdnId,
    pub issuer: PdnId,
}

/// The namespace was forgotten, or never imported here. Downcast from the
/// `anyhow::Error` of [`SyncNode::set_doc_contacts`].
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

/// Default of [`SpawnOptions::blob_collection_interval`].
const BLOB_COLLECTION_INTERVAL: Duration = Duration::from_secs(600);

/// Default of [`SpawnOptions::pod_change_settle`].
const POD_CHANGE_SETTLE: Duration = Duration::from_millis(50);

/// Chosen by name at spawn, with no default. Not read from the process
/// environment: several nodes spawn in one process, and a directory belongs
/// to one node.
#[derive(Debug, Clone)]
pub enum StorageConfig {
    Memory,
    /// `identities/<identity>/` (replica store and persisted author),
    /// `blobs/`, `node.key`, `lock`. Created owner-only when absent: the
    /// replica store holds namespace secrets and the blobs payload bytes in
    /// the clear.
    Directory(std::path::PathBuf),
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
/// [`SpawnOptions::memory`], [`SpawnOptions::on_directory`], or
/// [`SpawnOptions::for_product`].
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Required: a spawn that names neither memory nor a directory is not
    /// expressible.
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
/// store, registry and access book (ADR-0013). Data replicas are
/// addressed by issuer [`PdnId`] within the identity that holds them, and
/// entries by [`EntryPath`]. Every doc an identity opens joins the
/// periodic reconcile pass.
#[derive(Debug)]
pub struct SyncNode {
    router: Router,
    blobs: iroh_blobs::api::Store,
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
    /// Handed to every hosted identity's book; empty until a scenario takes
    /// the channel, so nothing accumulates unread.
    #[cfg(feature = "test-util")]
    pod_verdicts: crate::access::PodVerdictSink,
    /// Blob collection runs once set: at spawn on memory, at the host's
    /// [`start_blob_collection`](Self::start_blob_collection) on a
    /// directory.
    collecting: Arc<std::sync::atomic::AtomicBool>,
    /// What the passes report, so a scenario can order its absence
    /// assertions after runs that happened.
    #[cfg(feature = "test-util")]
    pass_probes: Arc<PassProbes>,
    storage: StorageConfig,
    retraction: Arc<RetractionTracker>,
    /// Taken once, by the runtime's consumer.
    retraction_verdicts: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>>>,
    /// Handed to every hosted identity's half; filled once, by the
    /// runtime's take.
    pod_notices: crate::pod::PodNoticeSink,
    /// Taken once, so a repeated `shutdown` is a no-op under a shared
    /// reference; both passes watch it.
    reconciler_stop: Mutex<Option<watch::Sender<()>>>,
    /// Cloned into every hosted identity's engine; the task at the other
    /// end holds the identity map weakly (`serve_co_located`).
    co_located_requests: pdn_store::engine::CoLocatedRequests,
    /// Released by `shutdown` with the stores, or with the process. `None`
    /// on a memory node.
    directory_lock: Option<std::fs::File>,
    /// `None` on a memory node, which comes back as nobody.
    address_book: Option<Arc<AddressBook>>,
}

/// How a tracked doc re-syncs, independent of the binding's serving
/// posture. `ContactsOnly` never joins the gossip swarm — every grantee
/// import: reconciliation is a grantee's only data path, and the swarm of a
/// data namespace is its issuer's device set.
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
    strategy: SyncStrategy,
    /// Whom a peer of this replica is dialed as when no contact names one
    /// — the swarm's own identity: the issuer for a data namespace, the
    /// identity for a store its devices share.
    default_identity: Identity,
}

/// What one [`SyncNode::import_namespace`] did, so that
/// [`SyncNode::undo_import_namespace`] undoes exactly that.
#[derive(Debug)]
pub struct NamespaceImport {
    identity: PdnId,
    issuer: PdnId,
    /// False for an import onto the replica the issuer already resolved
    /// to: it bound nothing, so undoing it forgets nothing.
    bound: bool,
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
/// replica store, the registry its data namespaces resolve through, the
/// book that judges its sessions, and the author its writes carry.
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
    /// At most one nudge in flight per namespace, so a tight poll loop
    /// cannot pile up attempts against one replica.
    nudges_in_flight: Mutex<HashSet<NamespaceId>>,
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
    /// One per pod, held through each derivation of that pod's contacts,
    /// from its fold to its last write: one that folded before a change
    /// would otherwise set its list after the one that folded after it.
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
                contacts,
                strategy,
                default_identity,
            },
        );
        Ok(())
    }

    /// Add `contacts` to a tracked store's, keeping those it has: one naming
    /// an endpoint and identity already listed is left as it was.
    fn add_contacts(&self, namespace: NamespaceId, contacts: Vec<Contact>) -> Result<()> {
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        let Some(entry) = docs.get_mut(&namespace) else {
            return Ok(());
        };
        for contact in contacts {
            let listed = entry.contacts.iter().any(|known| {
                known.addr.id == contact.addr.id && known.identity == contact.identity
            });
            if !listed {
                entry.contacts.push(contact);
            }
        }
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

    fn doc(&self, issuer: PdnId) -> Result<Doc> {
        self.registry
            .data_doc(issuer)?
            .ok_or_else(|| UnknownIssuer { issuer }.into())
    }
}

/// A node dropped without [`shutdown`](SyncNode::shutdown) still lets go of
/// its blob store: blob collection's task holds the store, and ends only once
/// the store has stopped.
impl Drop for SyncNode {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let blobs = self.blobs.clone();
        let _detached = runtime.spawn(async move {
            let _ = blobs.shutdown().await;
        });
    }
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
    /// ones (ADR-0011, ADR-0012), each dispatched as a raw bidirectional
    /// connection. An ALPN collision fails the spawn with [`AlpnTaken`]
    /// before anything binds. A handler's `accept` should return
    /// `Err(AcceptError)` rather than panic: a panic is contained per
    /// connection, but a `panic = "abort"` build still aborts the process.
    #[allow(clippy::too_many_lines)] // one node assembled in order, each part beside what it feeds
    pub async fn spawn_with(
        extra_protocols: Vec<ExtraProtocol>,
        options: SpawnOptions,
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
        // A memory node holds nothing an identity could come back for.
        let collecting = Arc::new(std::sync::atomic::AtomicBool::new(matches!(
            options.storage,
            StorageConfig::Memory
        )));
        let blobs_store = open_blob_store(&options, &identities, &collecting).await?;
        let gossip = Gossip::builder().spawn(endpoint.clone());

        let (retraction, retraction_verdicts) = RetractionTracker::new();
        let retraction = Arc::new(retraction);

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

        let mut router = Router::builder(endpoint)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs_store, None))
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
            pod_verdicts: Arc::default(),
            #[cfg(feature = "test-util")]
            pass_probes,
            collecting,
            storage: options.storage,
            retraction,
            retraction_verdicts: Mutex::new(Some(retraction_verdicts)),
            pod_notices: Arc::default(),
            reconciler_stop: Mutex::new(Some(reconciler_stop)),
            co_located_requests,
            directory_lock,
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
        let observer_tracker = Arc::clone(&self.retraction);
        let docs = self
            .docs_builder(identity, &access, &registry)?
            .capability_validator(capability_ingest_validator(&access, &registry))
            .rejection_observer(Arc::new(move |namespace, reject, peer| {
                observer_tracker.record_rejection(identity, namespace, reject, peer);
            }))
            .co_located_requests(self.co_located_requests.clone())
            .spawn(
                self.router.endpoint().clone(),
                self.blobs.clone(),
                self.gossip.clone(),
            )
            .await
            .map_err(|err| annotate_store_error(err, &self.storage))?;
        let api = docs.api().clone();
        let author = api.author_default().await?;
        self.retraction.track_author(identity, author);
        let stack = Arc::new(HostedStack {
            identity,
            docs,
            api,
            registry,
            access,
            author,
            tracked_docs: Mutex::new(HashMap::new()),
            nudges_in_flight: Mutex::new(HashSet::new()),
            announcements_in_flight: Mutex::new(HashSet::new()),
            converged_tombstones: Mutex::new(HashSet::new()),
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

    /// Sessions the periodic pass over the co-located pairs has opened —
    /// what shows that a pass over a quiet pair opens none. Counted
    /// per namespace, because a pair holds its data replica and its two
    /// connection stores alike, and a scenario about one of them cannot be
    /// read off a total the other two move.
    #[cfg(feature = "test-util")]
    pub fn co_located_pass_sessions(&self) -> u64 {
        self.co_located_sessions
            .lock()
            .map(|opened| opened.values().sum())
            .unwrap_or_default()
    }

    /// The same count for one namespace alone.
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
    /// PMS: the commit point of a create or a link. The
    /// replicas are flushed first, so the record never names one the store
    /// has not written, and the record is written beside, synced and
    /// renamed over, so a process that dies mid-write leaves none. The
    /// parent directory is not synced after the rename, so an OS crash or a
    /// power loss can take the record back after this returned `Ok`. A node
    /// in memory records nothing.
    pub async fn record_hosting(&self, identity: PdnId, pms: NamespaceId) -> Result<()> {
        let StorageConfig::Directory(root) = &self.storage else {
            return Ok(());
        };
        self.flush_replicas(identity, pms).await?;
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
        let StorageConfig::Directory(root) = &self.storage else {
            return Ok(Vec::new());
        };
        read_hosting_records(root)
    }

    /// Arm `identity`'s PMS for session classification: its device
    /// records decide who is one of its devices, and its data namespaces
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
        for tracked in stack.tracked_snapshot() {
            self.retraction
                .untrack_namespace(identity, tracked.doc.id());
        }
        stack.docs.engine().shutdown().await?;
        Ok(())
    }

    /// Register a connection for session classification: `own` carries the
    /// grants this identity issued, `peer_store` the counterparty's
    /// published device set.
    pub fn host_connection(
        &self,
        identity: PdnId,
        peer: PdnId,
        own: &ConnectionMetadataStore,
        peer_store: &ConnectionMetadataStore,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        stack
            .access
            .host_connection(peer, own.doc_handle(), peer_store.doc_handle())?;
        // The devices that hold one half hold the other, under the same
        // identity each: pointed at them too, `own` is dialed from here, so
        // the side that arms second reaches the first over both halves.
        if let Some(peer_half) = stack.tracked(peer_store.namespace())? {
            stack.add_contacts(own.namespace(), peer_half.contacts)?;
        }
        // After the arming, as `host_identity` starts the PMS's.
        stack.start_armed(own.namespace())?;
        stack.start_armed(peer_store.namespace())
    }

    /// Create a fresh doc and register it as `issuer`'s data namespace,
    /// held for `identity`.
    pub async fn create_namespace(&self, identity: PdnId, issuer: PdnId) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = self.new_doc(identity).await?;
        // `issuer` is minted fresh by the caller: nothing to displace.
        let _displaced = stack
            .registry
            .register_data(issuer, doc, ServingPosture::Serve)?;
        Ok(())
    }

    /// The device-replication import: a device of `identity` brings its
    /// own data replica up this way, joining its swarm. A namespace
    /// reached through a grant uses
    /// [`import_namespace_scoped`](Self::import_namespace_scoped). Binds
    /// nothing when the issuer already resolves to the ticket's replica.
    pub async fn import_namespace(
        &self,
        identity: PdnId,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let stack = self.require(identity)?;
        let namespace = ticket.capability.id();
        Self::guard_data_import(&stack, issuer, namespace)?;
        let current = stack.registry.data_doc(issuer)?.map(|doc| doc.id());
        if current == Some(namespace) {
            return Ok(NamespaceImport {
                identity,
                issuer,
                bound: false,
            });
        }
        let contacts = ticket.contacts();
        let doc = stack.api.import_namespace(ticket.capability).await?;
        stack.track(
            &doc,
            contacts.clone(),
            SyncStrategy::Swarm,
            crate::access::identity_of(issuer),
        )?;
        if let Some(previous) = current {
            self.forget_rebound(&stack, identity, previous).await;
        }
        let _displaced =
            stack
                .registry
                .register_data(issuer, doc.clone(), ServingPosture::Serve)?;
        let import = NamespaceImport {
            identity,
            issuer,
            bound: true,
        };
        if let Err(err) = doc
            .start_sync(contacts, crate::access::identity_of(issuer))
            .await
        {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        Ok(import)
    }

    /// The same grantee import as
    /// [`import_namespace_scoped`](Self::import_namespace_scoped); what the
    /// grant covers lives in the issuer's book, not in the import.
    pub async fn import_namespace_granted(
        &self,
        identity: PdnId,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(identity, issuer, ticket)
            .await
    }

    /// The grantee import: never joins the replica's gossip swarm, and
    /// re-serves it only to the issuer's published devices, whole, and to
    /// the audience identity's own devices per the locally replicated grant
    /// record.
    pub async fn import_namespace_scoped(
        &self,
        identity: PdnId,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(identity, issuer, ticket)
            .await
    }

    /// Refuses a ticket naming a tracked but not data-bound replica: honoring
    /// it would downgrade that store's sync strategy — leaving the gossip
    /// swarm, cutting its live path — on the word of whoever minted the
    /// ticket. Refuses any ticket under the identity's own id, and binds
    /// nothing when the issuer already resolves to the ticket's replica.
    async fn import_grantee_namespace(
        &self,
        identity: PdnId,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        // An identity holds its own data as its issuer: a grantee binding
        // under its own id would demote that replica, or drop it for another.
        if issuer == identity {
            return Err(anyhow::anyhow!(
                "{identity} cannot hold its own data under a grant"
            ));
        }
        let stack = self.require(identity)?;
        let contacts = ticket.contacts();
        let namespace = ticket.capability.id();
        Self::guard_data_import(&stack, issuer, namespace)?;
        let current = stack.registry.data_doc(issuer)?.map(|doc| doc.id());
        if current == Some(namespace) {
            return Ok(NamespaceImport {
                identity,
                issuer,
                bound: false,
            });
        }
        // The capability only — no `start_sync`, which would join the
        // swarm. The binding registers before the first sync, so even that
        // session is judged under the grantee rules.
        let doc = stack.api.import_namespace(ticket.capability).await?;
        stack.track(
            &doc,
            contacts.clone(),
            SyncStrategy::ContactsOnly,
            crate::access::identity_of(issuer),
        )?;
        if let Some(previous) = current {
            self.forget_rebound(&stack, identity, previous).await;
        }
        let _displaced =
            stack
                .registry
                .register_data(issuer, doc.clone(), ServingPosture::AudienceDevices)?;
        let import = NamespaceImport {
            identity,
            issuer,
            bound: true,
        };
        // A grantee binding stays outside the swarm, whatever joined the
        // replica before.
        if let Err(err) = doc.leave_gossip().await {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        if let Err(err) = doc
            .start_sync_scoped(contacts, crate::access::identity_of(issuer))
            .await
        {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        Ok(import)
    }

    /// Replace the reconciliation contacts of `issuer`'s data namespace —
    /// replacement is what lets a withdrawn device stop being dialed. Each
    /// contact names the identity it is dialed as.
    /// Refuses with [`UnknownIssuer`] whether the issuer was never bound or
    /// is bound but untracked: silently dropping the set would starve the
    /// replica unattributably.
    pub fn set_namespace_contacts(
        &self,
        identity: PdnId,
        issuer: PdnId,
        contacts: Vec<Contact>,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = stack
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.set_doc_contacts(identity, doc.id(), contacts)
            .map_err(|err| match err.downcast_ref::<UntrackedNamespace>() {
                Some(_untracked) => UnknownIssuer { issuer }.into(),
                None => err,
            })
    }

    /// Replace the reconciliation contacts of a device-shared store's doc: a
    /// ticket names only the devices of the side that minted it, so the
    /// caller records the devices it knows hold the replica. Refuses with
    /// [`UntrackedNamespace`] rather than dropping the set silently.
    pub fn set_doc_contacts(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
        contacts: Vec<Contact>,
    ) -> Result<()> {
        self.require(identity)?.set_contacts(namespace, contacts)
    }

    /// The observation side of
    /// [`set_namespace_contacts`](Self::set_namespace_contacts). Empty when
    /// the issuer resolves to no tracked replica.
    #[cfg(feature = "test-util")]
    pub fn namespace_contacts(&self, identity: PdnId, issuer: PdnId) -> Result<Vec<Contact>> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(Vec::new());
        };
        let Some(doc) = stack.registry.data_doc(issuer)? else {
            return Ok(Vec::new());
        };
        Ok(stack
            .tracked(doc.id())?
            .map(|tracked| tracked.contacts)
            .unwrap_or_default())
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

    /// Live records at `path` across authors — what every latest-wins read
    /// collapses, so this is the only way to assert one author per
    /// identity.
    #[cfg(feature = "test-util")]
    pub async fn live_record_count(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<usize> {
        let doc = self.require(identity)?.doc(issuer)?;
        let query = Query::all().key_exact(path.as_str().as_bytes());
        let mut stream = std::pin::pin!(doc.get_many(query).await?);
        let mut count = 0usize;
        while let Some(entry) = stream.next().await {
            let _live = entry?;
            count += 1;
        }
        Ok(count)
    }

    /// What the replica holds at `path`, without the nudge every product
    /// read makes: a scoped replica has no gossip path, so a read pokes a
    /// reconciliation, and an assertion that an entry arrived unasked has
    /// to observe the replica without asking.
    #[cfg(feature = "test-util")]
    pub async fn read_unnudged(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>> {
        let doc = self.require(identity)?.doc(issuer)?;
        read_payload(&doc, &self.blobs, path.as_str().as_bytes()).await
    }

    /// Open one session for `issuer`'s replica against `contact`, naming
    /// `caller` as the identity this side acts for — whoever this node
    /// hosts. The session a caller that claims an identity it does not
    /// hold produces, as [`write`](Self::write) past a grant produces an
    /// entry the issuer's gate refuses. `Err` is the peer's refusal.
    #[cfg(feature = "test-util")]
    pub async fn sync_as_for_test(
        &self,
        identity: PdnId,
        issuer: PdnId,
        contact: Contact,
        caller: Identity,
    ) -> Result<()> {
        let namespace = self
            .require(identity)?
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?
            .doc
            .id();
        self.sync_namespace_as_for_test(identity, namespace, contact, caller)
            .await
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

    /// [`sync_as_for_test`](Self::sync_as_for_test) for any replica
    /// `identity` holds, named by its namespace — a PMS or a
    /// connection metadata store as well as a data replica. No session
    /// access is asked on the dialing side, so it serves its replica whole.
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
                None,
                None,
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
    /// another role: its data replica, its PMS, a store of one of
    /// its connections. A device-shared store is classified on its ticket
    /// alone (Invariants 1 and 3), so a namespace that took that role by
    /// a counterparty's word would be served whole, past the grant that
    /// bounds it. The mirror of `guard_data_import` and of `open_doc`'s
    /// guard, on the path a counterparty's ticket takes.
    fn guard_shared_import(stack: &HostedStack, namespace: NamespaceId) -> Result<()> {
        let role = if stack.registry.binding_of(namespace)?.is_some() {
            "a data replica of this identity"
        } else if stack.registry.pod_of(namespace)?.is_some() {
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

    /// Refuses a ticket that would repurpose a namespace this identity
    /// already holds: a device-shared store taken as a data replica, or a
    /// replica taken for an issuer other than the one it is bound to.
    /// Both stand before the import writes anything, because the tracking
    /// entry is keyed by namespace and overwritten blind, while the
    /// registry's own refusal of a second issuer comes after that write
    /// and restores nothing.
    fn guard_data_import(stack: &HostedStack, issuer: PdnId, namespace: NamespaceId) -> Result<()> {
        if let Some((bound, _posture)) = stack.registry.binding_of(namespace)? {
            if bound != issuer {
                return Err(anyhow::anyhow!(
                    "namespace {namespace} is already bound to issuer {bound}; \
                     one namespace binds one issuer"
                ));
            }
        } else if let Some((pod, _store)) = stack.registry.pod_of(namespace)? {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a store of pod {pod}; \
                 a data import must not repurpose it"
            ));
        } else if stack.tracked(namespace)?.is_some() {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a device-shared replica of this identity; \
                 a data import must not repurpose it"
            ));
        }
        Ok(())
    }

    /// Drop the replica an import rebinds its issuer away from, while that
    /// binding still stands — the order `forget_namespace` keeps — so an
    /// issuer keeps one data binding and nothing outlives it. The import
    /// goes on whatever this answers: a replica that would not drop is
    /// already off the reconcile pass.
    async fn forget_rebound(&self, stack: &HostedStack, identity: PdnId, namespace: NamespaceId) {
        if let Err(err) = self.forget_doc(identity, namespace).await {
            tracing::warn!(%namespace, "a replica its issuer was rebound away from stayed in the store: {err:#}");
        }
        let _disarmed = stack.access.disarm_retractions(namespace);
        self.retraction.untrack_namespace(identity, namespace);
    }

    /// Leave exactly the state that preceded the import.
    pub async fn undo_import_namespace(&self, import: NamespaceImport) -> Result<()> {
        let NamespaceImport {
            identity,
            issuer,
            bound,
        } = import;
        if !bound {
            return Ok(());
        }
        self.forget_namespace(identity, issuer).await
    }

    /// Stop reconciling `issuer`'s replica in `identity`, drop it, and
    /// unregister the issuer, as one act — so operations afterwards fail
    /// with [`UnknownIssuer`] rather than as storage errors against a
    /// dropped replica.
    pub async fn forget_namespace(&self, identity: PdnId, issuer: PdnId) -> Result<()> {
        let stack = self.require(identity)?;
        // Drop first: a failed drop leaves the registration in place, so a
        // retry still resolves the issuer.
        let binding = stack
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        let namespace = binding.doc.id();
        self.forget_doc(identity, namespace).await?;
        let _unregistered = stack.registry.unregister_data(issuer)?;
        stack.access.disarm_retractions(namespace)?;
        self.retraction.untrack_namespace(identity, namespace);
        Ok(())
    }

    /// The registration probe for importers that memoize their imports:
    /// each import holds one more open handle on the replica, and the drop
    /// at the end of its life must find exactly one.
    pub fn data_namespace_of(&self, identity: PdnId, issuer: PdnId) -> Result<Option<NamespaceId>> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(None);
        };
        Ok(stack.registry.data_doc(issuer)?.map(|doc| doc.id()))
    }

    /// Create both stores of `pod` for `identity`, both or neither: they
    /// join the registry and the reconcile pass together, once both exist,
    /// and their sync starts once they are registered, so their first
    /// sessions are ones the book can judge.
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
        stack.track(
            &membership,
            Vec::new(),
            SyncStrategy::Swarm,
            stack.identity(),
        )?;
        stack.track(&records, Vec::new(), SyncStrategy::Swarm, stack.identity())?;
        Self::order_pod_stores(&stack, &membership, &records).await?;
        self.watch_pod(&stack, pod, &membership);
        membership.start_sync(Vec::new(), stack.identity()).await?;
        records.start_sync(Vec::new(), stack.identity()).await
    }

    /// Every dial of the record store follows an exchange of the membership
    /// store with the same counterpart, so the session that serves it folds
    /// the membership that exchange brought.
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

    /// Import both stores of `pod` from their write tickets: nothing new for
    /// a pod held on these stores, the pod again for its tombstone. Both
    /// stores' sync starts once they are registered, as at `create_pod`. A failed import drops nothing — a replica may predate
    /// it — and a retry lands on it.
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
    pub async fn import_pod_with(
        &self,
        identity: PdnId,
        pod: PodId,
        tickets: PodTickets,
        others: &[PodTickets],
    ) -> Result<PodCatchUp> {
        let stack = self.require(identity)?;
        let membership_contacts = contacts_of(
            &tickets.membership,
            others.iter().map(|other| &other.membership),
        )?;
        let records_contacts =
            contacts_of(&tickets.records, others.iter().map(|other| &other.records))?;
        let (membership_namespace, records_namespace) = (
            tickets.membership.capability.id(),
            tickets.records.capability.id(),
        );
        if membership_namespace == records_namespace {
            return Err(anyhow::anyhow!(
                "namespace {membership_namespace} cannot be both stores of pod {pod}"
            ));
        }
        if let Some(held) = stack.registry.pod(pod)? {
            if held.membership.id() != membership_namespace {
                return Err(anyhow::anyhow!(
                    "pod {pod} is held on membership store {}, not {membership_namespace}",
                    held.membership.id()
                ));
            }
            return match held.records {
                Some(records) if records.id() == records_namespace => {
                    self.start_pod(&stack, pod, &held.membership, &records)
                        .await
                }
                Some(records) => Err(anyhow::anyhow!(
                    "pod {pod} is held on record store {}, not {records_namespace}",
                    records.id()
                )),
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
        Self::guard_pod_import(&stack, membership_namespace)?;
        Self::guard_pod_import(&stack, records_namespace)?;
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
        let caught_up = PodCatchUp {
            membership: crate::private_metadata::watch_doc(membership).await?,
            records: crate::private_metadata::watch_doc(records).await?,
        };
        Self::order_pod_stores(stack, membership, records).await?;
        // Read before either starts: the membership store's first session
        // derives both stores' contacts again, from a fold that can list no
        // device of the inviter yet, and the record store would start with
        // none.
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

    /// Start a tracked store's sync with the contacts and the default
    /// identity it is tracked with.
    async fn start_tracked(tracked: Option<TrackedDoc>) -> Result<()> {
        let Some(tracked) = tracked else {
            return Ok(());
        };
        tracked
            .doc
            .start_sync(tracked.contacts, tracked.default_identity)
            .await
    }

    /// Refuses a ticket naming a namespace this identity already holds in
    /// any role: whatever bounded that replica's sessions before would stop
    /// bounding them once a ticket's word made it a pod's store.
    fn guard_pod_import(stack: &HostedStack, namespace: NamespaceId) -> Result<()> {
        let role = if stack.registry.binding_of(namespace)?.is_some() {
            "a data replica of this identity".to_owned()
        } else if let Some((other, _store)) = stack.registry.pod_of(namespace)? {
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
        doc.start_sync_scoped(contacts, minted_by).await
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
    /// their sessions. [`UnknownPod`] for a pod `identity` never held.
    pub async fn flush_pod(&self, identity: PdnId, pod: PodId) -> Result<PodFlush> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        let node = self.router.endpoint().id();
        let derived = pod_contacts(&stack, node, pod, &held, &[]).await?;
        let own = stack.identity();
        let others: Vec<Contact> = derived
            .contacts
            .unwrap_or_default()
            .into_iter()
            .filter(|contact| contact.identity != own)
            .collect();
        let (drawn, _recorded) = draw(others, Vec::new());
        let mut flush = PodFlush {
            since: std::time::SystemTime::now(),
            peers: drawn
                .iter()
                .map(|contact| NodeId::from_bytes(*contact.addr.id.as_bytes()))
                .collect(),
            sessions: Vec::new(),
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
            sync_pod_store(&stack, &tracked, drawn.clone(), Vec::new()).await?;
        }
        Ok(flush)
    }

    /// The membership `identity`'s replica of `pod`'s membership store
    /// folds into, payloads read as far as they have arrived.
    /// [`UnknownPod`] for a tombstone too.
    pub async fn pod_membership(&self, identity: PdnId, pod: PodId) -> Result<Membership> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        if held.records.is_none() {
            return Err(UnknownPod { pod }.into());
        }
        stack.access.fold_pod(pod, &held.membership).await
    }

    /// The highest sequence of `subject`'s chain that `identity`'s replica
    /// of `pod` holds, a tombstone's included; `0` for a pod not held here.
    pub async fn pod_chain_run(&self, identity: PdnId, pod: PodId, subject: PdnId) -> Result<u64> {
        let stack = self.require(identity)?;
        let Some(held) = stack.registry.pod(pod)? else {
            return Ok(0);
        };
        let membership = stack.access.fold_pod(pod, &held.membership).await?;
        Ok(membership.member(&subject).map_or(0, Member::run))
    }

    /// Wait, at most `timeout`, until `identity`'s replica of `pod` folds
    /// `identity` itself into a member: a session brings its own entries
    /// ahead of their payloads. [`CatchUpTimeout`] when it does not.
    pub async fn await_pod_member(
        &self,
        identity: PdnId,
        pod: PodId,
        timeout: Duration,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        // Subscribed before the first fold, so no change lands unseen.
        let mut changes = held.membership.subscribe().await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let folded = stack.access.fold_pod(pod, &held.membership).await?;
            if folded
                .member(&identity)
                .is_some_and(|member| member.state.member)
            {
                return Ok(());
            }
            match tokio::time::timeout_at(deadline, changes.next()).await {
                Ok(Some(_change)) => {}
                Ok(None) => anyhow::bail!("the membership store's change stream ended"),
                Err(_elapsed) => return Err(crate::CatchUpTimeout.into()),
            }
            // A burst — a session's entries, their payloads — folds once.
            while let Some(Some(_change)) = futures_lite::future::poll_once(changes.next()).await {}
        }
    }

    /// The record view over `identity`'s replica of `pod`'s record store,
    /// judged by the membership its membership store folds into; payloads
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
        let membership = stack.access.fold_pod(pod, &held.membership).await?;
        let entries = record_entries(&records, &self.blobs, query).await?;
        Ok(RecordView::new(&membership, entries))
    }

    /// Write `payload` at `key` into one of `pod`'s stores with
    /// `identity`'s author, checking nothing the key or the payload says:
    /// what the entry counts for is the fold's and the record view's.
    /// [`UnknownPod`] for a tombstone too.
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

    /// Once; a second take yields `None`. From the take on, every fold of a
    /// pod's membership store on this node — at a session's setup, at a
    /// read and at a run of the pod stores' pass — reports its verdicts.
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

    /// Reconcile one of `pod`'s stores with `contact` as a drawn contact
    /// is: through the engine, the record store after the membership
    /// store. Returns once the dial is asked for, not once it ends, and
    /// joins no swarm, so a store taken out of one stays out.
    #[cfg(feature = "test-util")]
    pub async fn sync_pod_with_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
        store: PodStore,
        contact: Contact,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = Self::pod_doc(&stack, pod, store)?;
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

    /// [`pod_membership`](Self::pod_membership) over a tombstone as well.
    #[cfg(feature = "test-util")]
    pub async fn held_pod_membership_for_test(
        &self,
        identity: PdnId,
        pod: PodId,
    ) -> Result<Membership> {
        let stack = self.require(identity)?;
        let held = stack.registry.pod(pod)?.ok_or(UnknownPod { pod })?;
        stack.access.fold_pod(pod, &held.membership).await
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

    /// Minting starts both stores' sync, as the store's share does for every
    /// replica. [`UnknownPod`] for a tombstone too.
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
            membership: held
                .membership
                .share(ShareMode::Write, addr_options)
                .await?,
            records: records.share(ShareMode::Write, addr_options).await?,
        })
    }

    /// Set exactly `devices` as the issuer's device set for retraction
    /// verdicts on `issuer`'s granted namespace.
    pub fn track_retraction_peers(
        &self,
        identity: PdnId,
        issuer: PdnId,
        devices: Vec<NodeId>,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = stack
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.retraction
            .track_namespace(identity, doc.id(), devices.into_iter().collect());
        Ok(())
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

    /// Once; a second take yields `None`.
    pub fn take_retraction_verdicts(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>> {
        self.retraction_verdicts
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Physically remove `author`'s record at `key` if its timestamp is at
    /// or below `bound` — no tombstone, which the gate would refuse and
    /// which would shadow the issuer's entries locally. Returns whether a
    /// record was removed.
    pub async fn retract_entry(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
        bound: u64,
    ) -> Result<bool> {
        let doc = self.require(identity)?.doc(issuer)?;
        doc.retract(author, key.to_vec(), bound).await
    }

    /// The marker's in-memory half: refuse re-ingest of `author`'s entries
    /// at `key` up to `bound`.
    pub fn arm_retraction(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        key: Vec<u8>,
        bound: u64,
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = stack
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        stack.access.arm_retraction(doc.id(), author, key, bound)
    }

    /// Whether `marker`'s version is armed on `issuer`'s replica and its
    /// removal ran. `None` when the issuer resolves to no replica here.
    pub fn retraction_applied(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
        marker: [u8; 32],
    ) -> Result<Option<bool>> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(None);
        };
        let Some(doc) = stack.registry.data_doc(issuer)? else {
            return Ok(None);
        };
        stack
            .access
            .retraction_applied(doc.id(), author, key, marker)
            .map(Some)
    }

    /// Record that `marker`'s removal ran after its arming, so a sweep
    /// skips it until a disarm.
    pub fn mark_retraction_applied(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
        marker: [u8; 32],
    ) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = stack
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        stack
            .access
            .mark_retraction_applied(doc.id(), author, key, marker)
    }

    /// Whether this identity holds exactly the entry `verdict` names —
    /// author, key, timestamp, content hash. A verdict's fields are the
    /// refusing peer's word and retraction is destructive; a version a
    /// newer own write already superseded must not be undone by a
    /// rejection still in flight for it.
    pub async fn holds_rejected_entry(
        &self,
        identity: PdnId,
        issuer: PdnId,
        verdict: &RetractionVerdict,
    ) -> Result<bool> {
        let doc = self.require(identity)?.doc(issuer)?;
        let query = Query::author(verdict.author).key_exact(&verdict.key);
        let Some(entry) = doc.get_one(query).await? else {
            return Ok(false);
        };
        Ok(entry.timestamp() == verdict.timestamp && entry.content_hash() == verdict.content_hash)
    }

    /// Take down what a dropped marker armed. An issuer resolving to no
    /// replica has nothing armed; not an error.
    pub fn disarm_retraction(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
    ) -> Result<()> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(());
        };
        let Some(doc) = stack.registry.data_doc(issuer)? else {
            return Ok(());
        };
        stack.access.disarm_retraction(doc.id(), author, key)
    }

    /// The reverse resolution a verdict consumer needs: whose data
    /// `namespace` is, as `identity` holds it. `None` when this identity
    /// holds no such replica — the verdict then addresses nothing here.
    pub fn issuer_of_held_namespace(
        &self,
        identity: PdnId,
        namespace: NamespaceId,
    ) -> Result<Option<PdnId>> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(None);
        };
        Ok(stack
            .registry
            .binding_of(namespace)?
            .map(|(issuer, _posture)| issuer))
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
        // The mirror of `guard_data_import`: tracking here is `Swarm`, so a
        // grantee import opened by mistake would be pulled into the swarm.
        if stack.registry.binding_of(namespace)?.is_some() {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a data replica of this identity; \
                 it cannot be opened as a device-shared store"
            ));
        }
        if let Some((pod, _store)) = stack.registry.pod_of(namespace)? {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a store of pod {pod}; \
                 it cannot be opened as a device-shared store"
            ));
        }
        if !holds_namespace(&stack.api, namespace).await? {
            return Ok(None);
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

    /// Read the replica store, to tell a store that still answers from one
    /// a full filesystem left refusing everything. The read asks one
    /// replica for its sync peers because that reaches the tables; the
    /// namespace listing and an empty entry query both keep answering long
    /// after the database has refused everything else.
    pub async fn check_replica_store(&self) -> Result<()> {
        for stack in self.stacks()? {
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
        // before the next pass. `host_identity` or `host_connection` starts
        // the sync once the store is armed.
        let doc = stack.api.import_namespace(ticket.capability).await?;
        stack.track(&doc, contacts, SyncStrategy::Swarm, minted_by)?;
        Ok(doc)
    }

    /// Untrack and drop a device-shared store's doc. Data namespaces go
    /// through [`forget_namespace`](Self::forget_namespace), which also
    /// unregisters the issuer, and a pod's stores through
    /// [`forget_pod`](Self::forget_pod).
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

    /// Commit the store's open write transaction, so what this node wrote
    /// is on disk before anything durable points at it: a read takes a
    /// snapshot, and taking one commits the batch first. Store-wide although
    /// it names a namespace; the read matches nothing — the commit is the
    /// point.
    pub async fn flush_replicas(&self, identity: PdnId, namespace: NamespaceId) -> Result<()> {
        let stack = self.require(identity)?;
        let doc = stack
            .tracked(namespace)?
            .map(|tracked| tracked.doc)
            .ok_or_else(|| {
                anyhow::anyhow!("namespace {namespace} is not tracked for identity {identity}")
            })?;
        let _committed = doc.get_many(Query::all().limit(0)).await?;
        Ok(())
    }

    pub(crate) fn blobs(&self) -> iroh_blobs::api::Store {
        self.blobs.clone()
    }

    pub async fn share_ticket(
        &self,
        identity: PdnId,
        issuer: PdnId,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let binding = self
            .require(identity)?
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        if binding.posture == ServingPosture::AudienceDevices {
            return Err(GranteeCannotShare { identity, issuer }.into());
        }
        let ticket = binding.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// A standalone author; an identity's own stores write with
    /// [`default_author`](Self::default_author) instead.
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

    pub async fn write(
        &self,
        identity: PdnId,
        issuer: PdnId,
        author: AuthorId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let doc = self.require(identity)?.doc(issuer)?;
        doc.set_bytes(author, path.as_str().as_bytes().to_vec(), payload.to_vec())
            .await?;
        Ok(())
    }

    /// `Ok(None)` both when no entry exists and when its payload has not
    /// been fetched yet — poll again. A grant-imported namespace is nudged
    /// first (non-blocking): the answer comes from the local replica at once.
    pub async fn read(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>> {
        let stack = self.require(identity)?;
        Self::nudge_scoped(&stack, issuer);
        let doc = stack.doc(issuer)?;
        read_payload(&doc, &self.blobs, path.as_str().as_bytes()).await
    }

    /// Fire-and-forget a filtered reconciliation of a `ContactsOnly`
    /// namespace; no-op otherwise. Debounced to one attempt in flight per
    /// namespace, or a tight poll loop piles up tasks against one replica.
    fn nudge_scoped(stack: &Arc<HostedStack>, issuer: PdnId) {
        let Ok(Some(binding)) = stack.registry.binding(issuer) else {
            return;
        };
        let namespace = binding.doc.id();
        let Ok(Some(tracked)) = stack.tracked(namespace) else {
            return;
        };
        if tracked.strategy != SyncStrategy::ContactsOnly {
            return;
        }
        {
            let Ok(mut in_flight) = stack.nudges_in_flight.lock() else {
                return;
            };
            if !in_flight.insert(namespace) {
                return;
            }
        }
        let stack = Arc::clone(stack);
        let _detached = tokio::spawn(async move {
            let _ = tracked
                .doc
                .start_sync_scoped(tracked.contacts, tracked.default_identity)
                .await;
            if let Ok(mut in_flight) = stack.nudges_in_flight.lock() {
                in_flight.remove(&namespace);
            }
        });
    }

    /// Entry metadata, record-level, optionally narrowed to `path_prefix`
    /// matching whole components (`contacts` matches `contacts/a`, not
    /// `contactsx/c`).
    pub async fn list(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>> {
        let stack = self.require(identity)?;
        Self::nudge_scoped(&stack, issuer);
        let doc = stack.doc(issuer)?;
        // Byte prefix as the coarse cut; component semantics per entry below.
        let query = Query::single_latest_per_key();
        let query = match path_prefix {
            Some(prefix) => query.key_prefix(prefix.as_str().as_bytes()),
            None => query,
        };
        let mut stream = std::pin::pin!(doc.get_many(query).await?);
        let mut entries = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            let Some(path) = path_of(entry.key()) else {
                continue;
            };
            if path_prefix.is_some_and(|prefix| !starts_with_components(&path, prefix)) {
                continue;
            }
            entries.push(EntryInfo {
                issuer,
                path,
                payload_len: entry.content_len(),
            });
        }
        Ok(entries)
    }

    /// The observation side of [`set_doc_contacts`](Self::set_doc_contacts).
    /// Empty when the namespace resolves to no tracked doc.
    #[cfg(feature = "test-util")]
    pub fn doc_contacts(&self, identity: PdnId, namespace: NamespaceId) -> Result<Vec<Contact>> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(Vec::new());
        };
        Ok(stack
            .tracked(namespace)?
            .map(|tracked| tracked.contacts)
            .unwrap_or_default())
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
    /// A memory node collects from its spawn.
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
        // Everything below runs whatever the router answers: a stop that
        // returned early would leave every engine, the blob store and the
        // directory lock held for the rest of the process.
        let routed = self.router.shutdown().await;
        // The router serves the dispatcher, not the engines, so each
        // identity's engine is shut down by name.
        let stacks: Vec<Arc<HostedStack>> = match self.identities.read() {
            Ok(hosted) => hosted.values().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
        };
        for stack in stacks {
            let _ = stack.docs.engine().shutdown().await;
        }
        // Explicit rather than on the last handle's drop: a node respawned
        // on the same directory would meet its predecessor's database lock.
        // Best-effort: a store already shut down answers with an error.
        let _ = self.blobs.shutdown().await;
        // With the stores, not with this value's drop: a detached task
        // holding the node alive a moment longer must not make a spawn on
        // the same directory read as a second running node.
        if let Some(lock) = &self.directory_lock {
            let _ = lock.unlock();
        }
        routed?;
        Ok(())
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

/// Answered from the listing: the fork reports "no such namespace" and
/// "the store could not answer" as one error of the same shape.
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
/// once `collecting` is set.
async fn open_blob_store(
    options: &SpawnOptions,
    identities: &Identities,
    collecting: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<iroh_blobs::api::Store> {
    let collection = GcConfig {
        interval: options.blob_collection_interval,
        add_protected: Some(protect_hosted(
            Arc::downgrade(identities),
            Arc::clone(collecting),
        )),
    };
    Ok(match &options.storage {
        StorageConfig::Memory => MemStore::new_with_opts(iroh_blobs::store::mem::Options {
            gc_config: Some(collection),
        })
        .into(),
        StorageConfig::Directory(directory) => {
            let root = directory.join(BLOBS_DIR);
            let mut store_options = iroh_blobs::store::fs::options::Options::new(&root);
            store_options.gc = Some(collection);
            FsStore::load_with_opts(root.join("blobs.db"), store_options)
                .await
                .with_context(|| format!("cannot open the blob store in {}", directory.display()))?
                .into()
        }
    })
}

/// Blob collection's protect callback: every payload a replica of an
/// identity the node hosts references, the node's one blob store being
/// every identity's. Every run before `collecting` is set is skipped.
fn protect_hosted(
    identities: std::sync::Weak<std::sync::RwLock<HashMap<PdnId, Arc<HostedStack>>>>,
    collecting: Arc<std::sync::atomic::AtomicBool>,
) -> ProtectCb {
    Arc::new(move |live: &mut HashSet<Hash>| {
        let identities = identities.clone();
        let collecting = Arc::clone(&collecting);
        Box::pin(async move {
            if !collecting.load(std::sync::atomic::Ordering::Acquire) {
                return ProtectOutcome::Abort;
            }
            let Some(identities) = identities.upgrade() else {
                return ProtectOutcome::Abort;
            };
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
            ProtectOutcome::Continue
        })
    })
}

/// Every payload hash the replicas of one identity's store reference.
async fn referenced_payloads(stack: &HostedStack) -> Result<Vec<Hash>> {
    stack.docs.engine().sync.content_hashes().await?.collect()
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
    let StorageConfig::Directory(directory) = storage else {
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
            directory: directory.clone(),
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
    let StorageConfig::Directory(directory) = storage else {
        return Ok((None, None, None));
    };
    let directory = directory.clone();
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
                let _ = match tracked.strategy {
                    SyncStrategy::ContactsOnly => {
                        tracked
                            .doc
                            .start_sync_scoped(tracked.contacts, tracked.default_identity)
                            .await
                    }
                    SyncStrategy::Swarm => {
                        tracked
                            .doc
                            .start_sync(tracked.contacts, tracked.default_identity)
                            .await
                    }
                };
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
    derive_pod_contacts(stack, node, pod, held, &[]).await;
    let stores = std::iter::once((PodStore::Membership, &held.membership)).chain(
        held.records
            .as_ref()
            .map(|records| (PodStore::Records, records)),
    );
    for (store, doc) in stores {
        let namespace = doc.id();
        let Ok(Some(tracked)) = stack.tracked(namespace) else {
            continue;
        };
        let recorded = stack
            .docs
            .engine()
            .sync
            .get_sync_peers(namespace)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let (contacts, recorded) = draw(tracked.contacts.clone(), recorded);
        probes.report(|| PodPassDraw {
            identity: stack.identity,
            pod,
            store,
            contacts: tracked.contacts.clone(),
            drawn: contacts.clone(),
            recorded: recorded
                .iter()
                .map(|peer| NodeId::from_bytes(*peer))
                .collect(),
        });
        let _ = sync_pod_store(stack, &tracked, contacts, recorded).await;
    }
}

/// Reconcile a pod store with `contacts` and `recorded`, in its swarm as
/// `tracked` read. A store `forget_pod` turned out of its swarm after that
/// read leaves the swarm again: the forget's own leave can run before this
/// sync joins.
async fn sync_pod_store(
    stack: &HostedStack,
    tracked: &TrackedDoc,
    contacts: Vec<Contact>,
    recorded: Vec<PeerIdBytes>,
) -> Result<()> {
    let swarm = tracked.strategy == SyncStrategy::Swarm;
    tracked
        .doc
        .sync_with_peers(contacts, recorded, tracked.default_identity, swarm)
        .await?;
    let turned_out = stack
        .tracked(tracked.doc.id())?
        .is_some_and(|now| now.strategy == SyncStrategy::ContactsOnly);
    if swarm && turned_out {
        tracked.doc.leave_gossip().await?;
    }
    Ok(())
}

/// Derive both of a pod's stores' contacts from its membership and set
/// them whole: as the stores are tracked, and as the engine dials each
/// peer, since a peer a gossip message or a recorded session names is
/// dialed as what contacts stated for it.
async fn derive_pod_contacts(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[NodeId],
) {
    let derivation = stack.pod_derivation(pod);
    let _derivation = derivation.lock().await;
    derive_pod_contacts_locked(stack, node, pod, held, met).await;
}

/// [`derive_pod_contacts`] with the pod's derivation lock held.
async fn derive_pod_contacts_locked(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[NodeId],
) {
    let derived = match pod_contacts(stack, node, pod, held, met).await {
        Ok(derived) => derived,
        Err(err) => {
            let identity = stack.identity;
            tracing::warn!(%identity, %pod, "deriving the pod's contacts failed: {err:#}");
            return;
        }
    };
    if held.records.is_some() {
        let identity = stack.identity;
        if let Some(seq) = derived.departed {
            stack.report(PodNotice::Departed { identity, pod, seq });
        }
        if derived.unlisted {
            stack.report(PodNotice::Unlisted { identity, pod });
        }
    }
    let Some(contacts) = derived.contacts else {
        return;
    };
    for doc in std::iter::once(&held.membership).chain(held.records.as_ref()) {
        let _ = stack.set_contacts(doc.id(), contacts.clone());
        let _ = stack
            .docs
            .engine()
            .state_contacts(doc.id(), contacts.clone())
            .await;
    }
}

/// Derive `held`'s contacts before its sync starts, so a reopened replica
/// dials each member's devices as that member — a ticket names every node
/// as the identity that minted it — and keep beside them each ticket
/// contact whose node they leave out: a replica that folds nobody yet, or a
/// tombstone past its convergence, has no other.
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
            if let Err(err) = sync_pod_store(stack, &tracked, contacts.clone(), Vec::new()).await {
                tracing::warn!(%pod, "dialing a newly listed device failed: {err:#}");
            }
        }
    }
}

/// Derive `pod`'s contacts again whenever its membership store changes —
/// at once when an entry or a payload arrives, since either may list a
/// device, and `settle` after anything else — so a newcomer is dialed as
/// the member it is from its first announcement on; a local write derives
/// in the write itself. Ends with the store's subscription or the
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
        while let Some(event) = events.next().await {
            // What is already queued joins this derivation; what arrives
            // during it derives again after it, so a store that never goes
            // quiet still derives.
            let mut met = Vec::new();
            let mut listing = note_event(event, &mut met);
            while let Some(Some(event)) = futures_lite::future::poll_once(events.next()).await {
                listing |= note_event(event, &mut met);
            }
            if !listing {
                let settled = tokio::time::Instant::now() + settle;
                while let Ok(Some(event)) = tokio::time::timeout_at(settled, events.next()).await {
                    if note_event(event, &mut met) {
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
            derive_pod_contacts(&stack, node, pod, &held, &met).await;
        }
    });
}

/// Notes the peer of a session that went through; whether the event may
/// list a device the contacts lack — an entry or a payload arriving, or
/// events dropped unread.
fn note_event(event: Result<pdn_store::engine::LiveEvent>, met: &mut Vec<NodeId>) -> bool {
    use pdn_store::engine::LiveEvent;
    match event {
        Ok(LiveEvent::SyncFinished(sync)) => {
            if sync.result.is_ok() {
                met.push(NodeId::from_bytes(*sync.peer.as_bytes()));
            }
            false
        }
        Ok(
            LiveEvent::InsertLocal { .. }
            | LiveEvent::PendingContentReady
            | LiveEvent::NeighborUp(_)
            | LiveEvent::NeighborDown(_),
        ) => false,
        Ok(LiveEvent::InsertRemote { .. } | LiveEvent::ContentReady { .. } | LiveEvent::Lagged)
        | Err(_) => true,
    }
}

/// A pod store's contacts: every device the statements of the pod's
/// current members list, each dialed as its member, and the identity's own
/// devices by its PMS, dialed as the identity; never this device as
/// this identity. `None` while the membership store folds into nobody: a
/// replica that holds nothing yet keeps the contacts its ticket gave it.
/// A tombstone drops the members' devices once a session with one of them
/// — any device of another than itself among `met` — went through. A
/// replica holding its record store again keeps them while its fold still
/// shows the departure: they are how a rejoin dials its inviter.
async fn pod_contacts(
    stack: &HostedStack,
    node: EndpointId,
    pod: PodId,
    held: &PodBinding,
    met: &[NodeId],
) -> Result<DerivedContacts> {
    let membership = &held.membership;
    let (folded, entries) = stack.access.fold_pod_entries(pod, membership).await?;
    if folded.identities().next().is_none() {
        return Ok(DerivedContacts {
            contacts: None,
            departed: None,
            unlisted: false,
        });
    }
    let departed =
        departure_past(&folded, &entries, &stack.identity).map(|past| Seq::new(past.seq));
    let own = stack.identity();
    let own_devices = stack.access.own_devices().await?;
    let this_device = NodeId::from_bytes(*node.as_bytes());
    let device = MemberDevice {
        node: this_device,
        author: stack.author,
    };
    let unlisted = folded
        .member(&stack.identity)
        .is_some_and(|member| member.state.member && !member.devices.contains(&device));
    let converged = {
        let mut converged = stack
            .converged_tombstones
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("tombstone lock poisoned"))?;
        if departed.is_some() && held.records.is_none() {
            if met
                .iter()
                .any(|peer| *peer != this_device && !own_devices.contains(peer))
            {
                converged.insert(pod);
            }
        } else {
            converged.remove(&pod);
        }
        converged.contains(&pod)
    };
    let mut devices: Vec<(NodeId, Identity)> = folded
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
        unlisted,
    })
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
                .contacts
                .into_iter()
                .map(|contact| (contact.addr.id, contact.addr))
                .collect()
        })
        .unwrap_or_default())
}

/// What [`pod_contacts`] derives from one fold.
struct DerivedContacts {
    /// `None` while the membership store folds into nobody.
    contacts: Option<Vec<Contact>>,
    /// The sequence of the identity's own departure.
    departed: Option<Seq>,
    /// The identity is a member, and no statement of its own lists this
    /// device with the author it writes with here.
    unlisted: bool,
}

/// At most [`POD_RECONCILE_PEERS`] of a store's contacts and recorded
/// peers, drawn afresh; a recorded peer a contact names counts once, as the
/// contact, which names the identity it is dialed as.
fn draw(contacts: Vec<Contact>, recorded: Vec<PeerIdBytes>) -> (Vec<Contact>, Vec<PeerIdBytes>) {
    enum Peer {
        Contact(Contact),
        Recorded(PeerIdBytes),
    }
    let mut peers: Vec<Peer> = recorded
        .into_iter()
        .filter(|peer| {
            !contacts
                .iter()
                .any(|contact| contact.addr.id.as_bytes() == peer)
        })
        .map(Peer::Recorded)
        .collect();
    peers.extend(contacts.into_iter().map(Peer::Contact));
    peers.shuffle(&mut rand::rng());
    let (mut contacts, mut recorded) = (Vec::new(), Vec::new());
    for peer in peers.into_iter().take(POD_RECONCILE_PEERS) {
        match peer {
            Peer::Contact(contact) => contacts.push(contact),
            Peer::Recorded(peer) => recorded.push(peer),
        }
    }
    (contacts, recorded)
}

/// The sessions a [`SyncNode::flush_pod`] dialed, as each store finishes
/// them.
pub struct PodFlush {
    /// A session started before it may have missed the latest writes. A
    /// clock stepping back past it leaves the wait to its bound.
    since: std::time::SystemTime,
    peers: HashSet<NodeId>,
    sessions: Vec<
        std::pin::Pin<
            Box<dyn futures_core::Stream<Item = Result<pdn_store::engine::LiveEvent>> + Send>,
        >,
    >,
}

impl std::fmt::Debug for PodFlush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PodFlush")
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

impl PodFlush {
    /// Whether a session with one of the dialed devices went through on
    /// each store within `timeout`; `false` at once when none was dialed.
    pub async fn wait(self, timeout: Duration) -> bool {
        if self.peers.is_empty() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        for mut events in self.sessions {
            loop {
                match tokio::time::timeout_at(deadline, events.next()).await {
                    Ok(Some(Ok(pdn_store::engine::LiveEvent::SyncFinished(sync))))
                        if sync.result.is_ok()
                            && sync.started >= self.since
                            && self
                                .peers
                                .contains(&NodeId::from_bytes(*sync.peer.as_bytes())) =>
                    {
                        break;
                    }
                    Ok(Some(_other)) => {}
                    Ok(None) | Err(_) => return false,
                }
            }
        }
        true
    }
}

/// The hold [`SyncNode::pause_next_pod_start_for_test`] puts on a pod
/// start: `reached` once its membership store's sync has started.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub struct PodStartPause {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

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
    pub recorded: Vec<NodeId>,
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

/// Sessions the pass has opened, per namespace: a pair holds its data
/// replica and its two connection stores, and each is reconciled on its
/// own.
type CoLocatedPassSessions = Arc<Mutex<HashMap<NamespaceId, u64>>>;

/// What a co-located pair looked like before its last successful pass
/// session: how many writes each side's replica had taken, and how many
/// each side's connection stores had, both sides in the pair's canonical
/// order. A pass skips the pair only while all four stand still.
///
/// The two replicas are not compared: one held under a claim-scoped grant
/// lacks for good what the grant withholds, and the grant lives in the
/// connection stores, where it changes with no write to the namespace.
#[derive(PartialEq, Eq, Clone, Copy)]
struct PairReading {
    source_writes: u64,
    target_writes: u64,
    source_rights: u64,
    target_rights: u64,
    /// A pod's record store beside its membership store, the two read as
    /// one pair.
    source_records: u64,
    target_records: u64,
}

/// The writes taken by the connection stores this identity holds toward
/// `peer` — where a grant between the two is written and where it arrives.
/// `0` when the two are not connected: then no grant binds them.
async fn rights_writes(stack: &Arc<HostedStack>, peer: PdnId) -> u64 {
    let Ok(Some((own, peer_doc))) = stack.access.connection_stores(peer) else {
        return 0;
    };
    let sync = &stack.docs.engine().sync;
    let (Ok(own), Ok(peer_doc)) = (sync.writes(own).await, sync.writes(peer_doc).await) else {
        return 0;
    };
    own.saturating_add(peer_doc)
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
    // The grant is read only for a data replica: a device-shared store is
    // served on its ticket (Invariants 1 and 3), so no grant governs what
    // flows through it. Reading the connection stores for one of those
    // would read them for themselves, and their own reconciliation would
    // then keep invalidating their own memo.
    let governed = matches!(first.registry.binding_of(namespace), Ok(Some(_)))
        || matches!(second.registry.binding_of(namespace), Ok(Some(_)));
    let (source_rights, target_rights) = if governed {
        (
            rights_writes(first, second.identity).await,
            rights_writes(second, first.identity).await,
        )
    } else {
        (0, 0)
    };
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
        source_rights,
        target_rights,
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

fn path_of(key: &[u8]) -> Option<EntryPath> {
    let s = std::str::from_utf8(key).ok()?;
    EntryPath::new(s).ok()
}

/// Both are validated paths, so a byte prefix plus a component boundary is
/// exactly component semantics.
fn starts_with_components(path: &EntryPath, prefix: &EntryPath) -> bool {
    match path.as_str().strip_prefix(prefix.as_str()) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each draw reaches at most five of a store's contacts and recorded
    /// peers, a recorded peer a contact names counting once, as the
    /// contact, and draws differ from run to run until every peer has been
    /// reached.
    #[test]
    fn a_draw_reaches_at_most_five_peers_afresh_each_run() {
        let key = |seed: u8| SecretKey::from_bytes(&[seed; 32]).public();
        let contacts: Vec<Contact> = (1..=6)
            .map(|seed| {
                Contact::new(
                    EndpointAddr::new(key(seed)),
                    Identity::from_bytes([seed; 32]),
                )
            })
            .collect();
        let named = *key(1).as_bytes();
        let recorded = vec![named, *key(7).as_bytes(), *key(8).as_bytes()];
        let mut reached = HashSet::new();
        for _run in 0..200 {
            let (drawn, drawn_recorded) = draw(contacts.clone(), recorded.clone());
            assert_eq!(drawn.len() + drawn_recorded.len(), POD_RECONCILE_PEERS);
            assert!(
                !drawn_recorded.contains(&named),
                "a recorded peer a contact names was drawn as recorded"
            );
            reached.extend(drawn.iter().map(|contact| *contact.addr.id.as_bytes()));
            reached.extend(drawn_recorded);
        }
        assert_eq!(reached.len(), 8, "a peer went undrawn for 200 runs");
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
}
