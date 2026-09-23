//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. Externally supplied protocols — pdn-node's pairing and
//! linking dialogues (ADR-0011, ADR-0012) — register on the same endpoint at
//! spawn; a narrow dial handle serves their dial sides. The registration
//! point is protocol-agnostic: the ceremonies' semantics live in pdn-node.

use std::{
    collections::{HashMap, HashSet},
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
    store::{fs::FsStore, mem::MemStore},
    BlobsProtocol, ALPN as BLOBS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc, DocsApi,
    },
    protocol::{Docs, DocsDispatch},
    store::Query,
    AuthorId, Contact, DocTicket, Identity, NamespaceId, ALPN as DOCS_ALPN,
};
use pdn_types::{EntryInfo, EntryPath, NodeId, PdnId};
use tokio::sync::oneshot;

use crate::{
    access::{capability_ingest_validator, session_access_provider, AccessBook},
    connection_metadata::ConnectionMetadataStore,
    private_metadata::PrivateMetadataStore,
    registry::{Registry, ServingPosture},
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
            connectivity: Connectivity::Direct,
            replica_cache_budget_bytes: DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
        }
    }

    /// Under `directory`, direct paths — what the container stand runs on.
    pub fn on_directory(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            storage: StorageConfig::Directory(directory.into()),
            reconcile_interval: RECONCILE_INTERVAL,
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
    /// Sessions [`reconcile_co_located`] has opened, so a scenario can
    /// assert that a pass over a converged pair opens none.
    #[cfg(feature = "test-util")]
    co_located_sessions: CoLocatedPassSessions,
    storage: StorageConfig,
    retraction: Arc<RetractionTracker>,
    /// Taken once, by the runtime's consumer.
    retraction_verdicts: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>>>,
    /// Taken once, so a repeated `shutdown` is a no-op under a shared
    /// reference.
    reconciler_stop: Mutex<Option<oneshot::Sender<()>>>,
    /// Cloned into every hosted identity's engine; the task at the other
    /// end holds the identity map weakly (`serve_co_located`).
    co_located_requests: pdn_store::engine::CoLocatedRequests,
    /// Released by `shutdown` with the stores, or with the process. `None`
    /// on a memory node.
    directory_lock: Option<std::fs::File>,
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
    /// The namespace of the identity's private metadata directory.
    pub directory: NamespaceId,
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
    /// Announcements of a local write already on their way to a co-located
    /// identity. One write that finds the pair busy is queued by the engine
    /// and replayed, so the rest of a batch buys nothing but a task, a pipe
    /// and a message through the actor's inbox each.
    announcements_in_flight: Mutex<HashSet<(NamespaceId, Identity)>>,
}

impl HostedStack {
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

        let (secret_key, directory_lock) = prepare_storage(&options.storage).await?;

        let endpoint = bind_endpoint(secret_key, options.connectivity).await?;
        let blobs_store: iroh_blobs::api::Store = match &options.storage {
            StorageConfig::Memory => MemStore::default().into(),
            StorageConfig::Directory(directory) => FsStore::load(directory.join(BLOBS_DIR))
                .await
                .with_context(|| format!("cannot open the blob store in {}", directory.display()))?
                .into(),
        };
        let gossip = Gossip::builder().spawn(endpoint.clone());

        let (retraction, retraction_verdicts) = RetractionTracker::new();
        let retraction = Arc::new(retraction);

        let identities: Identities = Arc::default();
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
        let (reconciler_stop, stop) = oneshot::channel();
        let co_located_sessions: CoLocatedPassSessions = Arc::default();
        let _detached = tokio::spawn(reconcile_pass(
            options.reconcile_interval,
            Arc::clone(&identities),
            Arc::clone(&co_located_sessions),
            stop,
        ));
        Ok(Self {
            router,
            blobs: blobs_store,
            gossip,
            identities,
            cache_budget_bytes: options.replica_cache_budget_bytes,
            #[cfg(feature = "test-util")]
            co_located_sessions: Arc::clone(&co_located_sessions),
            storage: options.storage,
            retraction,
            retraction_verdicts: Mutex::new(Some(retraction_verdicts)),
            reconciler_stop: Mutex::new(Some(reconciler_stop)),
            co_located_requests,
            directory_lock,
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
        });
        let mut hosted = self
            .identities
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("hosted identities lock poisoned"))?;
        hosted.insert(identity, stack);
        Ok(())
    }

    /// Sessions the periodic pass over the co-located pairs has opened —
    /// what shows that a pass over a converged pair opens none. Counted
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
    /// takes a share cut from a smaller set, so the sum passes the budget
    /// until the next start cuts every share from the whole set.
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

    /// Record `identity` as hosted here, with `directory` as its private
    /// metadata directory: the commit point of a create or a link. The
    /// replicas are flushed first, so the record never names one the store
    /// has not written, and the record is written beside and renamed over,
    /// so a failure leaves none. A node in memory records nothing.
    pub async fn record_hosting(&self, identity: PdnId, directory: NamespaceId) -> Result<()> {
        let StorageConfig::Directory(root) = &self.storage else {
            return Ok(());
        };
        self.flush_replicas(identity, directory).await?;
        let own = identity_directory(root, identity);
        tokio::task::spawn_blocking(move || write_hosting_record(&own, directory))
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

    /// Arm `identity`'s directory for session classification: its device
    /// records decide who is one of its devices, and its data namespaces
    /// serve fail-closed from here on. The directory's sync starts here,
    /// after the arming, so its first session is one the book can judge.
    pub fn host_identity(&self, identity: PdnId, directory: &PrivateMetadataStore) -> Result<()> {
        let stack = self.require(identity)?;
        stack.access.arm_directory(directory.doc_handle())?;
        stack.start_armed(directory.namespace())
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
        stack.access.disarm_directory()?;
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
        // After the arming, as `host_identity` starts the directory's.
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
    /// re-serves it only to the devices of the grant's audience identity per
    /// the locally replicated grant record.
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
        let stack = self.require(identity)?;
        let mut docs = stack
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        let entry = docs
            .get_mut(&namespace)
            .ok_or(UntrackedNamespace { namespace })?;
        entry.contacts = contacts;
        Ok(())
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
        let stack = self.require(identity)?;
        let namespace = stack
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?
            .doc
            .id();
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
    /// another role: its data replica, its directory, a store of one of
    /// its connections. A device-shared store is classified on its ticket
    /// alone (Invariants 1 and 3), so a namespace that took that role by
    /// a counterparty's word would be served whole, past the grant that
    /// bounds it. The mirror of `guard_data_import` and of `open_doc`'s
    /// guard, on the path a counterparty's ticket takes.
    fn guard_shared_import(stack: &HostedStack, namespace: NamespaceId) -> Result<()> {
        let role = if stack.registry.binding_of(namespace)?.is_some() {
            "a data replica of this identity"
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
        match stack.registry.binding_of(namespace)? {
            Some((bound, _posture)) => {
                if bound != issuer {
                    return Err(anyhow::anyhow!(
                        "namespace {namespace} is already bound to issuer {bound}; \
                         one namespace binds one issuer"
                    ));
                }
            }
            None => {
                if stack.tracked(namespace)?.is_some() {
                    return Err(anyhow::anyhow!(
                        "namespace {namespace} is a device-shared replica of this identity; \
                         a data import must not repurpose it"
                    ));
                }
            }
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
        // Drop first: the reverse order opens a window in which the replica
        // is alive but unknown to the book, and so served whole; a failed
        // drop leaves the registration in place, so a retry still resolves
        // the issuer.
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
    /// unregisters the issuer.
    pub async fn forget_doc(&self, identity: PdnId, namespace: NamespaceId) -> Result<()> {
        let Some(stack) = self.stack(identity)? else {
            return Ok(());
        };
        stack.untrack(namespace)?;
        stack.api.drop_doc(namespace).await?;
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

/// The identities the storage directory holds, one subdirectory each:
/// what a store's share of the cache budget is cut from, so no host
/// states a count of its own (ADR-0013).
/// The identities whose subdirectory holds a hosting record, plus
/// `opening` when its own does not yet.
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

fn write_hosting_record(own: &std::path::Path, directory: NamespaceId) -> Result<()> {
    use std::io::Write as _;
    let path = own.join(HOSTING_RECORD_FILE);
    let staged = own.join(format!("{HOSTING_RECORD_FILE}.tmp"));
    let context = || format!("cannot write the hosting record {}", path.display());
    {
        let mut file = std::fs::File::create(&staged).with_context(context)?;
        file.write_all(directory.to_string().as_bytes())
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
        let directory = text
            .trim()
            .parse::<NamespaceId>()
            .with_context(|| format!("cannot parse the hosting record {}", path.display()))?;
        let store_present = entry.path().join(REPLICA_STORE_FILE).is_file();
        recorded.push(RecordedHosting {
            identity,
            directory,
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

/// One subdirectory per identity, each holding that identity's replica
/// store (`docs.redb`), its persisted author (`default-author`) and, once
/// a create or link commits, its hosting record (`directory`).
const IDENTITIES_DIR: &str = "identities";
/// The store pdn-store opens in an identity's subdirectory.
const REPLICA_STORE_FILE: &str = "docs.redb";
/// The namespace of the identity's private metadata directory, as text: a
/// start hosts exactly the identities whose subdirectory holds one.
const HOSTING_RECORD_FILE: &str = "directory";
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
) -> Result<(Option<SecretKey>, Option<std::fs::File>)> {
    let StorageConfig::Directory(directory) = storage else {
        return Ok((None, None));
    };
    let directory = directory.clone();
    let (key, lock) = tokio::task::spawn_blocking(move || {
        provision_directory(&directory)?;
        let lock = lock_directory(&directory)?;
        let key = read_or_generate_node_key(&directory)?;
        anyhow::Ok((key, lock))
    })
    .await
    .context("the storage directory could not be provisioned")??;
    Ok((Some(key), Some(lock)))
}

/// If `PDN_BIND_ADDR` holds an IP address the endpoint binds it with an
/// ephemeral port (the just recipes set `127.0.0.1` to keep test traffic on
/// loopback); unset, all interfaces. Only the widest `connectivity` takes
/// iroh's `N0` preset, which publishes a record under this node's id; the
/// narrower two take the relay mode alone over `Minimal`.
async fn bind_endpoint(
    secret_key: Option<SecretKey>,
    connectivity: Connectivity,
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
/// docs with their contacts (the engine unions in the peers it recorded),
/// and reconcile the co-located pairs whose replicas differ. A failed
/// request is retried by the next pass. Ends when `stop` is sent or its
/// sender is dropped with the node.
async fn reconcile_pass(
    interval: Duration,
    identities: Identities,
    co_located_sessions: CoLocatedPassSessions,
    mut stop: oneshot::Receiver<()>,
) {
    let mut reconciled: HashMap<(PdnId, PdnId, NamespaceId), PairReading> = HashMap::new();
    while tokio::time::timeout(interval, &mut stop).await.is_err() {
        let stacks: Vec<Arc<HostedStack>> = match identities.read() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(_poisoned) => continue,
        };
        for stack in &stacks {
            for tracked in stack.tracked_snapshot() {
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
    }
}

/// Sessions the pass has opened, per namespace: a pair holds its data
/// replica and its two connection stores, and each is reconciled on its
/// own.
type CoLocatedPassSessions = Arc<Mutex<HashMap<NamespaceId, u64>>>;

/// What a co-located pair looked like when it last reconciled: how many
/// writes each side's replica had taken, and how many each side's
/// connection stores had, both sides in the pair's canonical order. A pass
/// skips the pair only while all four stand still.
///
/// Equality of the two replicas is not the question and cannot be: a
/// replica held under a claim-scoped grant is poorer than the issuer's by
/// construction, so the two are never equal and no digest of them says
/// otherwise. What decides whether anything is owed is the grant, and it
/// lives in the connection stores, where it changes with no write to the
/// namespace at all.
#[derive(PartialEq, Eq, Clone, Copy)]
struct PairReading {
    source_writes: u64,
    target_writes: u64,
    source_rights: u64,
    target_rights: u64,
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
    Some(PairReading {
        source_writes: first.docs.engine().sync.writes(namespace).await.ok()?,
        target_writes: second.docs.engine().sync.writes(namespace).await.ok()?,
        source_rights,
        target_rights,
    })
}

/// Reconcile each pair of hosted identities holding one namespace, and
/// leave alone a pair where nothing has moved since they last reconciled —
/// a pass over a quiet namespace must not accumulate sessions.
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
                let (first, second) = ordered(source, target);
                let pair = (first.identity, second.identity, namespace);
                let Some(reading) = pair_reading(source, target, namespace).await else {
                    continue;
                };
                if reconciled.get(&pair) == Some(&reading) {
                    continue;
                }
                if let Ok(mut opened) = opened.lock() {
                    *opened.entry(namespace).or_default() += 1;
                }
                let reconciliation = source
                    .docs
                    .engine()
                    .sync_in_process(target.docs.engine(), namespace, target.identity())
                    .await;
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
