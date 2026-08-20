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
    endpoint::{presets, Connection},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, SecretKey, Watcher as _,
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
    protocol::Docs,
    store::Query,
    AuthorId, DocTicket, NamespaceId, ALPN as DOCS_ALPN,
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

/// An operation addressed a data namespace this node does not host: `issuer`
/// has no created or imported namespace here. Downcast from the
/// `anyhow::Error` of [`SyncNode::read`] / [`SyncNode::write`] /
/// [`SyncNode::share_ticket`].
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("data namespace not bound on this node: {issuer}")]
pub struct UnknownIssuer {
    /// The issuer whose data namespace was addressed.
    pub issuer: PdnId,
}

/// A protocol supplied to [`SyncNode::spawn_with`]: the ALPN it
/// answers under, and the handler dispatched for connections arriving on it.
pub type ExtraProtocol = (Vec<u8>, Box<dyn DynProtocolHandler>);

/// The ALPNs of the built-in protocols — blob transfer, gossip, document
/// sync. Reserved: an externally supplied protocol claiming one of these is
/// refused at spawn with [`AlpnTaken`].
pub const BUILT_IN_ALPNS: [&[u8]; 3] = [BLOBS_ALPN, GOSSIP_ALPN, DOCS_ALPN];

/// A spawn was handed an extra protocol whose ALPN is already taken — by a
/// built-in protocol ([`BUILT_IN_ALPNS`]) or by another extra in the same
/// call. Downcast from the `anyhow::Error` of [`SyncNode::spawn_with`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("protocol ALPN already taken: {}", String::from_utf8_lossy(.alpn))]
pub struct AlpnTaken {
    /// The colliding ALPN.
    pub alpn: Vec<u8>,
}

/// A spawn addressed a directory another running node holds: the replica
/// store inside takes an exclusive lock, and it is taken. The refused start
/// leaves the running node untouched. Downcast from the `anyhow::Error` of
/// the spawn entries; the underlying lock error stays in the chain as the
/// cause.
#[derive(Debug, Clone, thiserror::Error)]
#[error("storage directory {} is held by another running node", directory.display())]
pub struct DirectoryHeld {
    /// The directory both nodes were pointed at.
    pub directory: std::path::PathBuf,
}

/// Wraps an externally supplied protocol handler so a panic in its `accept`
/// cannot escape into iroh's router accept loop, where a panicking handler
/// task is fatal and tears the whole node down. A caught panic drops just
/// that one connection; the dialer may observe a clean end-of-stream rather
/// than an error (the unwind drops the handler's `SendStream`, which
/// finishes it). Does not survive a `panic = "abort"` build.
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

/// How often the periodic reconcile pass re-requests a sync for every doc
/// this node holds open — the default of
/// [`SpawnOptions::reconcile_interval`].
///
/// Gossip broadcasts are best-effort and the rescue triggers ride that same
/// gossip; without this pass a late write can starve until some unrelated
/// contact. Each pass re-dials a doc's import-time contacts plus the peers
/// the engine has recorded as useful; the import contacts matter because
/// the engine records a peer only after one *successful* exchange — without
/// them a replica whose initial exchange died would starve permanently.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Where a node keeps its state — chosen by name at spawn, with no default:
/// the runtime's production consumer is a mobile application that embeds it
/// and passes a directory inside its own sandbox, and the workspace's suites
/// want memory and say so. Deliberately not read from the process
/// environment: several nodes spawn in one process, and a directory belongs
/// to one node.
#[derive(Debug, Clone)]
pub enum StorageConfig {
    /// Everything in memory; the node's state ends with the process.
    Memory,
    /// The node's state lives under this directory: `docs/` holds the
    /// fork's replica store and its persisted author, `blobs/` the payload
    /// bytes, `node.key` the endpoint's secret key. Created with owner-only
    /// permissions when absent — the replica store holds namespace secrets
    /// and the blobs payload bytes in the clear, so the boundary sits on
    /// the directory. One running node per directory: the replica store's
    /// exclusive lock refuses a second ([`DirectoryHeld`]).
    Directory(std::path::PathBuf),
}

/// Spawn-time configuration of the node stack ([`SyncNode::spawn_with`]):
/// where the node stores its state — required, no default — plus tuning.
/// Build with [`SpawnOptions::memory`] or [`SpawnOptions::on_directory`].
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Where the node keeps its state. Required: a spawn that names
    /// neither memory nor a directory is not expressible.
    pub storage: StorageConfig,
    /// How often the periodic reconcile pass re-requests a sync for every
    /// doc this node holds open (default [`RECONCILE_INTERVAL`]).
    pub reconcile_interval: Duration,
}

impl SpawnOptions {
    /// Options for a node whose state lives in memory and ends with the
    /// process — what the workspace's in-process suites run on.
    pub fn memory() -> Self {
        Self {
            storage: StorageConfig::Memory,
            reconcile_interval: RECONCILE_INTERVAL,
        }
    }

    /// Options for a node whose state lives under `directory`
    /// ([`StorageConfig::Directory`]).
    pub fn on_directory(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            storage: StorageConfig::Directory(directory.into()),
            reconcile_interval: RECONCILE_INTERVAL,
        }
    }
}

/// One running node: iroh endpoint, gossip, blob store, and the docs
/// engine, with data replicas addressed by their issuer [`PdnId`] and
/// entries by [`EntryPath`]s. One node hosts the store sets of any number of
/// identities. Every doc the node opens joins a periodic reconcile pass
/// ([`SpawnOptions::reconcile_interval`]). Externally supplied protocols
/// join the same endpoint at spawn ([`SyncNode::spawn_with`]);
/// their dial sides and the node's own address are reached through
/// [`SyncNode::dial_handle`].
///
/// Both directions of a session are enforced through the node's access
/// book. Reads: every session is classified — full for a replica identity's
/// own devices and connection audiences, capability-filtered for granted
/// counterparties, refused as not-hosted otherwise. Writes: the fork's
/// ingest hook (ADR-0008) is installed with the book's validator — on a
/// replica data-bound to a hosted identity, a synced entry is admitted only
/// from the issuer's own devices or, per claim, per the sender's session
/// write set; refused entries are dropped before persisting. Enforcement
/// arms per identity by registration ([`SyncNode::host_identity`] /
/// [`SyncNode::host_connection`]) and per replica by
/// [`SyncNode::import_namespace_scoped`]; a node that registers nothing
/// serves — and admits — any ticket holder the whole replica.
///
/// Storage is chosen at spawn ([`SpawnOptions::storage`]), by name: in
/// memory, ending with the process, or under a directory — the replicas,
/// the blobs, the node's one author and its endpoint key all live there, so
/// a node spawned on the same directory comes back as the same node.
#[derive(Debug)]
pub struct SyncNode {
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: DocsApi,
    registry: Arc<Registry>,
    /// Session classification material: hosted identities' directories and
    /// connection pairs, consulted by the access provider wired into the
    /// docs engine at spawn.
    access: Arc<AccessBook>,
    /// Every doc handle this node opened — data namespaces and device-shared
    /// stores alike — keyed by namespace for the periodic reconcile pass, so
    /// a re-import replaces its entry rather than accreting a second one.
    tracked_docs: Arc<Mutex<HashMap<NamespaceId, TrackedDoc>>>,
    /// Namespaces with a before-access nudge currently in flight
    /// ([`nudge_scoped`](Self::nudge_scoped)) — at most one spawned attempt
    /// per namespace at a time, so a tight poll loop cannot pile up
    /// concurrent attempts against one replica.
    nudges_in_flight: Arc<Mutex<HashSet<NamespaceId>>>,
    /// Provisional-write tracking behind the fork's rejection observer; the
    /// runtime deposits what it takes to judge one (granted namespaces, the
    /// issuers' devices, own authors) and consumes the verdicts.
    retraction: Arc<RetractionTracker>,
    /// The verdict stream's receiving half, taken once by the runtime's
    /// consumer ([`SyncNode::take_retraction_verdicts`]).
    retraction_verdicts: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>>>,
    /// Ends the periodic reconcile pass when dropped — with the node — or by
    /// the explicit send in [`SyncNode::shutdown`]. Taken once: a second
    /// `shutdown` call finds `None` and skips the send, making the method
    /// idempotent under a shared reference.
    reconciler_stop: Mutex<Option<oneshot::Sender<()>>>,
    /// The node's exclusive hold on its storage directory
    /// ([`lock_directory`]) — released by [`shutdown`](Self::shutdown),
    /// with the stores, or when the node is dropped or the process ends.
    /// `None` on a memory node.
    directory_lock: Option<std::fs::File>,
}

/// How a tracked doc re-syncs — independent of the binding's serving
/// posture. `Swarm` joins the replica's gossip swarm — the issuer's own
/// devices and the device-shared stores. `ContactsOnly` re-syncs with the
/// ticket's contacts alone and never joins the swarm — every grantee
/// import: gossip broadcasts entries past the
/// access book, so the swarm of a data namespace is its issuer's device
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncStrategy {
    Swarm,
    ContactsOnly,
}

/// One doc under the periodic reconcile pass: the handle, the contacts its
/// import ticket carried (empty for docs created here), and the sync
/// strategy. The engine records a peer as useful only after one successful
/// exchange, so the contacts are the only recovery path for a replica whose
/// initial exchange died.
#[derive(Debug, Clone)]
struct TrackedDoc {
    doc: Doc,
    contacts: Vec<EndpointAddr>,
    strategy: SyncStrategy,
}

/// What one [`SyncNode::import_namespace`] did, carried back to the caller so
/// that [`SyncNode::undo_import_namespace`] can undo exactly that and nothing
/// more. Opaque on purpose: it holds the fork's replica handle, which stays
/// behind this layer, and only the node consumes it.
#[derive(Debug)]
pub struct NamespaceImport {
    /// The issuer whose binding the import wrote.
    issuer: PdnId,
    /// The namespace the import bound the issuer to.
    imported: NamespaceId,
    /// The binding the import displaced — `None` if the issuer was free.
    displaced: Option<crate::registry::DataBinding>,
    /// The tracking entry the import's `track` replaced — `None` if the
    /// namespace was untracked. The undo puts the previous entry back and
    /// re-aligns the swarm membership with it, so a failed import cannot
    /// leave the replica syncing under the wrong strategy.
    displaced_tracking: Option<TrackedDoc>,
}

/// The dial side of a node's protocols, handed out by
/// [`SyncNode::dial_handle`]. Wraps the node's iroh endpoint but exposes
/// only what a dial needs — connect out, read the node's own address and
/// wire id — never the endpoint's lifecycle, which stays the node's own
/// ([`SyncNode::shutdown`]).
#[derive(Debug, Clone)]
pub struct DialHandle {
    endpoint: Endpoint,
}

impl DialHandle {
    /// Open a connection to `addr` under `alpn`, as the dial side of an
    /// extra protocol. The peer must serve `alpn` — a built-in protocol or
    /// an extra it registered at spawn — or the dial fails.
    pub async fn connect(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
        Ok(self.endpoint.connect(addr, alpn).await?)
    }

    /// This node's own address — its wire id plus the paths peers can reach
    /// it on — to hand to a peer out of band (a pairing QR, say) as the
    /// dial target for the reverse direction.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// This node's wire id; [`SyncNode::node_id`] reports the same value as
    /// a [`NodeId`].
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }
}

impl SyncNode {
    /// Spawn the full stack with no externally supplied protocols,
    /// configured by `options` — where the state lives is a required part
    /// of it ([`SpawnOptions::storage`]).
    pub async fn spawn(options: SpawnOptions) -> Result<Self> {
        Self::spawn_with(Vec::new(), options).await
    }

    /// The full-control spawn: extra protocols plus configuration, served
    /// on the same endpoint next to the built-in ones (ADR-0011, ADR-0012).
    /// A connection arriving under a registered extra ALPN is dispatched to
    /// its handler as a raw bidirectional connection — not a document-sync
    /// session. ALPNs must be unique across [`BUILT_IN_ALPNS`] and the
    /// extras; a collision fails the spawn with [`AlpnTaken`] before
    /// anything binds.
    ///
    /// A handler's `accept` should return `Err(AcceptError)` rather than
    /// panic: a panic is contained per connection, but a `panic = "abort"`
    /// build still aborts the process.
    pub async fn spawn_with(
        extra_protocols: Vec<ExtraProtocol>,
        options: SpawnOptions,
    ) -> Result<Self> {
        // Checked before the endpoint binds: an extra silently replacing a
        // built-in handler would leave a node that looks alive and never
        // syncs.
        let mut taken: HashSet<&[u8]> = BUILT_IN_ALPNS.into_iter().collect();
        for (alpn, _handler) in &extra_protocols {
            if !taken.insert(alpn.as_slice()) {
                return Err(AlpnTaken { alpn: alpn.clone() }.into());
            }
        }

        // A configured directory is provisioned before anything binds, and
        // the endpoint's key comes out of it — a node that persists its
        // stores but not its key would come back under a fresh wire id
        // while its device records and tickets all name the old one. The
        // directory lock comes first of all: one running node per
        // directory, refused by name ([`DirectoryHeld`]).
        let (secret_key, directory_lock) = match &options.storage {
            StorageConfig::Memory => (None, None),
            StorageConfig::Directory(directory) => {
                provision_directory(directory)?;
                let lock = lock_directory(directory)?;
                (Some(read_or_generate_node_key(directory)?), Some(lock))
            }
        };

        let endpoint = bind_endpoint(secret_key).await?;
        let blobs_store: iroh_blobs::api::Store = match &options.storage {
            StorageConfig::Memory => MemStore::default().into(),
            StorageConfig::Directory(directory) => FsStore::load(directory.join(BLOBS_DIR))
                .await
                .with_context(|| format!("cannot open the blob store in {}", directory.display()))?
                .into(),
        };
        let gossip = Gossip::builder().spawn(endpoint.clone());

        // The access book and registry exist before the engine so the
        // session access provider can close over them; the blob handle is
        // set right after the spawn, before any session can arrive.
        let registry = Arc::new(Registry::default());
        let access = Arc::new(AccessBook::default());
        let (retraction, retraction_verdicts) = RetractionTracker::new();
        let retraction = Arc::new(retraction);
        let observer_tracker = Arc::clone(&retraction);
        let docs_builder = match &options.storage {
            StorageConfig::Memory => Docs::memory(),
            StorageConfig::Directory(directory) => Docs::persistent(directory.join(DOCS_DIR)),
        };
        let docs = match docs_builder
            .session_access_provider(session_access_provider(
                Arc::clone(&access),
                Arc::clone(&registry),
            ))
            .capability_validator(capability_ingest_validator(
                Arc::clone(&access),
                Arc::clone(&registry),
            ))
            .rejection_observer(Arc::new(move |namespace, reject, peer| {
                observer_tracker.record_rejection(namespace, reject, peer);
            }))
            .spawn(endpoint.clone(), blobs_store.clone(), gossip.clone())
            .await
        {
            Ok(docs) => docs,
            Err(err) => {
                // A node that never reaches its caller closes what it
                // opened: the blob store holds its database open, and the
                // directory lock releases on drop, so a retry on the same
                // directory in this process would meet this attempt's own
                // abandoned store — and wait on it rather than be refused.
                let _ = blobs_store.shutdown().await;
                return Err(annotate_store_error(err, &options.storage));
            }
        };
        let docs_api = docs.api().clone();
        access.set_blobs(blobs_store.clone());
        let mut router = Router::builder(endpoint)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs_store, None))
            .accept(GOSSIP_ALPN, gossip)
            .accept(DOCS_ALPN, docs);
        // Wrapped so a panic in a handler cannot escape into iroh's accept
        // loop, where it is fatal to the whole node (`PanicGuarded`).
        for (alpn, handler) in extra_protocols {
            router = router.accept(alpn, PanicGuarded { inner: handler });
        }
        let router = router.spawn();
        let tracked_docs: Arc<Mutex<HashMap<NamespaceId, TrackedDoc>>> = Arc::default();
        let (reconciler_stop, stop) = oneshot::channel();
        let _detached = tokio::spawn(reconcile_pass(
            options.reconcile_interval,
            Arc::clone(&tracked_docs),
            stop,
        ));
        Ok(Self {
            router,
            blobs: blobs_store,
            docs: docs_api,
            registry,
            access,
            tracked_docs,
            nudges_in_flight: Arc::default(),
            retraction,
            retraction_verdicts: Mutex::new(Some(retraction_verdicts)),
            reconciler_stop: Mutex::new(Some(reconciler_stop)),
            directory_lock,
        })
    }

    /// Register `identity`'s directory for session classification: its
    /// device records decide which callers are this identity's own devices
    /// — full view of its replicas — and arm fail-closed serving for its
    /// data namespace.
    pub fn host_identity(&self, identity: PdnId, directory: &PrivateMetadataStore) -> Result<()> {
        self.access.host_identity(identity, directory.doc_handle())
    }

    /// Remove `identity`'s directory from session classification — the
    /// rollback counterpart of [`host_identity`](Self::host_identity), for
    /// a ceremony that armed the identity and then failed. Registered
    /// connections are untouched.
    pub fn unhost_identity(&self, identity: PdnId) -> Result<()> {
        self.access.unhost_identity(identity)
    }

    /// Register a connection of `identity` toward `peer` for session
    /// classification: `own` carries the grants this identity issued (read
    /// at session setup), `peer_store` the counterparty's published device
    /// set, which resolves a caller's node id to `peer`.
    pub fn host_connection(
        &self,
        identity: PdnId,
        peer: PdnId,
        own: &ConnectionMetadataStore,
        peer_store: &ConnectionMetadataStore,
    ) -> Result<()> {
        self.access
            .host_connection(identity, peer, own.doc_handle(), peer_store.doc_handle())
    }

    /// Create a fresh doc and register it as the data namespace of `issuer`.
    pub async fn create_namespace(&self, issuer: PdnId) -> Result<()> {
        let doc = self.new_doc().await?;
        // A registration cannot already exist: `issuer` is minted fresh by
        // the caller that provisions it, so there is nothing to displace or
        // restore.
        let _displaced = self
            .registry
            .register_data(issuer, doc, ServingPosture::Serve)?;
        Ok(())
    }

    /// Import a doc shared via `ticket` and register it as the data
    /// namespace of `issuer` — the device-replication path: the issuer's own
    /// devices bring the replica up this way, and a device that holds it may
    /// re-serve it to the next device. A namespace reached through a
    /// cross-identity **grant** uses
    /// [`import_namespace_granted`](Self::import_namespace_granted) or
    /// [`import_namespace_scoped`](Self::import_namespace_scoped) instead.
    ///
    /// Returns what the import did, undoable through
    /// [`undo_import_namespace`](Self::undo_import_namespace); a binding the
    /// import displaced travels in the token, not dropped here.
    ///
    /// A ticket naming a replica that is tracked but not data-bound — a
    /// directory or a connection metadata store — is refused: a data import
    /// must not hijack a device-shared replica's tracking.
    pub async fn import_namespace(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let displaced_tracking = self.guard_data_import(ticket.capability.id())?;
        // Capability first, sync last, with the binding between them — the
        // order the grantee import already keeps. A session arriving at a
        // namespace the book does not know is classified `Full`, so a
        // replica syncing before its binding is recorded would serve whole
        // what the binding scopes.
        let contacts = ticket.nodes.clone();
        let doc = self.docs.import_namespace(ticket.capability).await?;
        let imported = doc.id();
        self.track(&doc, contacts.clone(), SyncStrategy::Swarm)?;
        let displaced =
            match self
                .registry
                .register_data(issuer, doc.clone(), ServingPosture::Serve)
            {
                Ok(displaced) => displaced,
                Err(err) => {
                    // The one-namespace-one-issuer rejection must not clobber
                    // the rightful issuer's tracking (the swarm was not joined
                    // here, so re-inserting the entry is the whole restore).
                    if let Some(previous) = displaced_tracking {
                        let _ = self.restore_tracking(previous).await;
                    }
                    return Err(err);
                }
            };
        let import = NamespaceImport {
            issuer,
            imported,
            displaced,
            displaced_tracking,
        };
        if let Err(err) = doc.start_sync(contacts).await {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        Ok(import)
    }

    /// Import a doc shared via `ticket` as a **whole-store grant** of
    /// `issuer`: access arrives through a grant, not through being a device
    /// of the issuer, so — unlike
    /// [`import_namespace`](Self::import_namespace) — this node never joins
    /// the replica's gossip swarm, and re-serves it only to the devices of
    /// the grant's audience identity, per the locally replicated grant
    /// record. Classified reconciliation with the tracked contacts is the
    /// only data path; what makes this grant whole-store rather than scoped
    /// lives entirely in the issuer's book, not in the import.
    ///
    /// Returns what the import did, undoable through
    /// [`undo_import_namespace`](Self::undo_import_namespace).
    pub async fn import_namespace_granted(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(issuer, ticket).await
    }

    /// Import a doc shared via `ticket` as a **scoped** data namespace of
    /// `issuer`: access arrives through a grant, not through being a device
    /// of the issuer. A scoped import never joins the replica's gossip swarm
    /// — capability-filtered reconciliation with the tracked contacts is its
    /// only data path — and the slice is re-served only to the devices of
    /// the grant's audience identity, per the locally replicated grant
    /// record.
    ///
    /// Returns what the import did, undoable through
    /// [`undo_import_namespace`](Self::undo_import_namespace).
    pub async fn import_namespace_scoped(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(issuer, ticket).await
    }

    /// The one grantee import behind
    /// [`import_namespace_granted`](Self::import_namespace_granted) and
    /// [`import_namespace_scoped`](Self::import_namespace_scoped):
    /// `AudienceDevices` re-serving, `ContactsOnly` sync. The two public
    /// names differ only in what the caller was granted — a distinction the
    /// issuer's book enforces per session.
    ///
    /// Like the device-replication import, refuses a ticket naming a
    /// tracked but not data-bound replica (a directory, a connection
    /// metadata store): honoring it would downgrade that store's sync
    /// strategy — leaving the gossip swarm, cutting its live path — on the
    /// word of whoever minted the ticket.
    async fn import_grantee_namespace(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let contacts = ticket.nodes.clone();
        let displaced_tracking = self.guard_data_import(ticket.capability.id())?;
        // Import the capability only — no automatic start_sync, which would
        // join the gossip swarm. The grantee binding registers *before* the
        // first sync, so even the very first session is judged under the
        // grantee rules.
        let doc = self.docs.import_namespace(ticket.capability).await?;
        let imported = doc.id();
        self.track(&doc, contacts.clone(), SyncStrategy::ContactsOnly)?;
        let displaced =
            match self
                .registry
                .register_data(issuer, doc.clone(), ServingPosture::AudienceDevices)
            {
                Ok(displaced) => displaced,
                Err(err) => {
                    // The one-namespace-one-issuer rejection must not clobber
                    // the rightful issuer's tracking (the swarm was not joined
                    // here, so re-inserting the entry is the whole restore).
                    if let Some(previous) = displaced_tracking {
                        let _ = self.restore_tracking(previous).await;
                    }
                    return Err(err);
                }
            };
        // The capability, tracking, and binding are in place; the swarm
        // leave and the first sync remain. If either fails, roll the whole
        // import back through the same undo the caller would use, rather
        // than propagate with the binding half-installed and the displaced
        // one lost.
        let import = NamespaceImport {
            issuer,
            imported,
            displaced,
            displaced_tracking,
        };
        // Swarm membership follows the recorded strategy: a device-
        // replicated import downgraded to a grantee binding leaves the
        // swarm now, so the membership cannot outlive the strategy. A no-op
        // for a replica that never joined.
        if let Err(err) = doc.leave_gossip().await {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        if let Err(err) = doc.start_sync_scoped(contacts).await {
            let _ = self.undo_import_namespace(import).await;
            return Err(err);
        }
        Ok(import)
    }

    /// Set the reconciliation contacts for `issuer`'s data namespace,
    /// replacing the previous set — devices of the grant's audience and of
    /// the issuer, derived by the caller from the durable device records on
    /// every sweep. Replacement is what lets a contact leave: a device
    /// withdrawn from the records is simply absent from the next derived
    /// set, so it stops being dialed. The periodic reconcile pass and the
    /// before-access nudge dial exactly this set (the engine unions in
    /// peers it has recorded as useful), so a granted replica catches up
    /// from any device in it while the others are offline.
    ///
    /// Refuses with [`UnknownIssuer`] when `issuer` resolves to no tracked
    /// replica — whether it was never bound or is bound-but-untracked (the
    /// registry and the tracking map are separate: [`forget_doc`] untracks
    /// without unregistering). Both are the same failure to the caller —
    /// "nowhere to record these contacts" — so both surface, rather than a
    /// silent `Ok` that drops them and starves the replica unattributably.
    pub fn set_namespace_contacts(&self, issuer: PdnId, contacts: Vec<EndpointAddr>) -> Result<()> {
        let doc = self
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        let entry = docs.get_mut(&doc.id()).ok_or(UnknownIssuer { issuer })?;
        entry.contacts = contacts;
        Ok(())
    }

    /// The reconciliation contacts currently tracked for `issuer`'s data
    /// namespace — the observation side of
    /// [`set_namespace_contacts`](Self::set_namespace_contacts), so a
    /// scenario asserts what a sweep derived instead of sleeping and
    /// guessing. Behind the `test-util` feature and absent from every
    /// product build. Empty when the issuer resolves to no tracked replica.
    #[cfg(feature = "test-util")]
    pub fn namespace_contacts(&self, issuer: PdnId) -> Result<Vec<EndpointAddr>> {
        let Some(doc) = self.registry.data_doc(issuer)? else {
            return Ok(Vec::new());
        };
        let docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        Ok(docs
            .get(&doc.id())
            .map(|entry| entry.contacts.clone())
            .unwrap_or_default())
    }

    /// The number of documents currently tracked by the periodic reconcile
    /// pass — every doc registered by a create/import and not yet
    /// forgotten. Behind the `test-util` feature and absent from every
    /// product build: a scenario asserts this count is unchanged after a
    /// cancelled or failed attempt, the only anchor available when the
    /// attempt's replica has no other name a scenario can check by.
    #[cfg(feature = "test-util")]
    pub fn tracked_doc_count(&self) -> Result<usize> {
        let docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("tracked docs lock poisoned"))?;
        Ok(docs.len())
    }

    /// How many live records `issuer`'s replica holds at `path`, across
    /// all authors — where every ordinary read collapses to the latest one.
    /// Behind the `test-util` feature and absent from every product build:
    /// a scenario asserts a rewrite after a restart replaced its
    /// predecessor rather than accreting beside it under a second author,
    /// which no latest-wins read can tell apart.
    #[cfg(feature = "test-util")]
    pub async fn live_record_count(&self, issuer: PdnId, path: &EntryPath) -> Result<usize> {
        let doc = self.doc(issuer)?;
        let query = Query::all().key_exact(path.as_str().as_bytes());
        let mut stream = std::pin::pin!(doc.get_many(query).await?);
        let mut count = 0usize;
        while let Some(entry) = stream.next().await {
            let _live = entry?;
            count += 1;
        }
        Ok(count)
    }

    /// The shared precondition of both data-namespace imports: hand back
    /// the tracking entry the import is about to replace, refusing when the
    /// namespace is tracked but not data-bound — that replica is a
    /// device-shared store, and a data import must not hijack its tracking.
    fn guard_data_import(&self, namespace: NamespaceId) -> Result<Option<TrackedDoc>> {
        let displaced_tracking = {
            let docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("tracked docs lock poisoned"))?;
            docs.get(&namespace).cloned()
        };
        if displaced_tracking.is_some() && self.registry.binding_of(namespace)?.is_none() {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a device-shared replica on this node; \
                 a data import must not repurpose it"
            ));
        }
        Ok(displaced_tracking)
    }

    /// Put back a tracking entry a failed act displaced, and re-align the
    /// swarm membership with its strategy: a `ContactsOnly` entry leaves
    /// the swarm now (best-effort — the restore must not fail over it), a
    /// `Swarm` entry re-joins on the next reconcile pass by itself.
    async fn restore_tracking(&self, tracking: TrackedDoc) -> Result<()> {
        self.track(&tracking.doc, tracking.contacts.clone(), tracking.strategy)?;
        if tracking.strategy == SyncStrategy::ContactsOnly {
            let _ = tracking.doc.leave_gossip().await;
        }
        Ok(())
    }

    /// Undo an import: leave exactly the state that preceded it, touching
    /// nothing the import did not touch. A free issuer is unbound again and
    /// the imported replica dropped; a replaced binding is put back, and the
    /// imported replica is dropped **only** when it is a different one —
    /// with one namespace per issuer (ADR-0009) an import under an
    /// already-bound issuer resolves to the very replica the binding names,
    /// and dropping it would destroy the data the restore exists to preserve
    /// (`drop_doc` is permanent).
    pub async fn undo_import_namespace(&self, import: NamespaceImport) -> Result<()> {
        let NamespaceImport {
            issuer,
            imported,
            displaced,
            displaced_tracking,
        } = import;
        let Some(previous) = displaced else {
            return self.forget_namespace(issuer).await;
        };
        let previous_namespace = previous.doc.id();
        let _replaced = self.registry.register_binding(issuer, previous)?;
        if imported != previous_namespace {
            self.forget_doc(imported).await?;
        } else if let Some(tracking) = displaced_tracking {
            // Same replica: the import's `track` replaced the previous
            // entry, and the restored binding must sync under the entry it
            // was recorded with — `Swarm` re-joins on the next reconcile
            // pass, `ContactsOnly` leaves the swarm now.
            self.restore_tracking(tracking).await?;
        }
        Ok(())
    }

    /// Forget the data namespace of `issuer`: stop reconciling the replica,
    /// drop it, and remove the issuer's registration, as one act.
    /// Operations addressed to `issuer` afterwards fail with
    /// [`UnknownIssuer`]. Dropping the replica without unregistering is
    /// deliberately not offered: the issuer would keep resolving to a
    /// dropped replica, and its operations would fail as storage errors
    /// instead of the distinguishable refusal.
    pub async fn forget_namespace(&self, issuer: PdnId) -> Result<()> {
        // Drop first, unregister second: the reverse order holds a window
        // in which the replica is alive but unknown to the book. A failed
        // drop leaves the registration in place, so a retry still resolves
        // the issuer instead of erroring on a half-forgotten one.
        let binding = self
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        let namespace = binding.doc.id();
        self.forget_doc(namespace).await?;
        let _unregistered = self.registry.unregister_data(issuer)?;
        // Retraction state leaves with the replica: the entries its markers
        // address are gone, and a rejection naming them judges nothing.
        self.access.disarm_retractions(namespace)?;
        self.retraction.untrack_namespace(namespace);
        Ok(())
    }

    /// The namespace `issuer` currently resolves to here, `None` when it is
    /// unbound — the registration probe for importers that memoize their own
    /// imports. A memo entry whose issuer no longer resolves marks an import
    /// to redo, not one to skip; an issuer already resolving to the very
    /// namespace at hand marks a registration to adopt, not to re-import —
    /// each import holds one more open handle on the replica, and the drop
    /// at the end of its life must find exactly one.
    pub fn data_namespace_of(&self, issuer: PdnId) -> Result<Option<NamespaceId>> {
        Ok(self.registry.data_doc(issuer)?.map(|doc| doc.id()))
    }

    /// Record `author` as one of this node's own writers, so the
    /// provisional-write tracker recognizes its entries.
    pub fn track_writer_author(&self, author: AuthorId) {
        self.retraction.track_author(author);
    }

    /// Track the granted namespace of `issuer` for provisional-write
    /// verdicts, with exactly `devices` counting as the issuer's device set
    /// — replacing any previous set. Refuses with [`UnknownIssuer`] when
    /// the issuer resolves to no replica here.
    pub fn track_retraction_peers(&self, issuer: PdnId, devices: Vec<NodeId>) -> Result<()> {
        let doc = self
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.retraction
            .track_namespace(doc.id(), devices.into_iter().collect());
        Ok(())
    }

    /// Take the provisional-write verdict stream — once: the runtime's
    /// consumer owns it, and a second take yields `None`.
    pub fn take_retraction_verdicts(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>> {
        self.retraction_verdicts
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Physically remove `author`'s record at `key` in `issuer`'s replica,
    /// if its timestamp is at or below `bound` — the retraction act. No
    /// tombstone: the set shrinks, and re-ingest of the removed entry is
    /// refused only while a matching armed retraction says so
    /// ([`arm_retraction`](Self::arm_retraction)). Returns whether a record
    /// was removed.
    pub async fn retract_entry(
        &self,
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
        bound: u64,
    ) -> Result<bool> {
        let doc = self.doc(issuer)?;
        doc.retract(author, key.to_vec(), bound).await
    }

    /// Arm the ingest refusal for `author`'s entries at `key` in `issuer`'s
    /// replica up to `bound` — the marker's in-memory half, so a retracted
    /// entry cannot flap back from a sibling that still holds it.
    pub fn arm_retraction(
        &self,
        issuer: PdnId,
        author: AuthorId,
        key: Vec<u8>,
        bound: u64,
    ) -> Result<()> {
        let doc = self
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.access.arm_retraction(doc.id(), author, key, bound)
    }

    /// Whether this node holds exactly the entry `verdict` names in
    /// `issuer`'s replica: same author, same key, same timestamp, same
    /// content hash.
    ///
    /// A verdict's fields arrive from the peer that refused the write, and
    /// retraction is destructive, so nothing but the local record makes them
    /// true. A fabricated timestamp names no record here; neither does one a
    /// newer own write has already superseded — writing again after a refusal
    /// is the way back, and it must not be undone by a rejection still in
    /// flight for the version it replaced.
    pub async fn holds_rejected_entry(
        &self,
        issuer: PdnId,
        verdict: &RetractionVerdict,
    ) -> Result<bool> {
        let doc = self.doc(issuer)?;
        let query = Query::author(verdict.author).key_exact(&verdict.key);
        let Some(entry) = doc.get_one(query).await? else {
            return Ok(false);
        };
        Ok(entry.timestamp() == verdict.timestamp && entry.content_hash() == verdict.content_hash)
    }

    /// Take down the ingest refusal armed for `author`'s entries at `key` in
    /// `issuer`'s replica — what a dropped marker leaves behind. Arming is
    /// in memory and only ever widens ([`arm_retraction`](Self::arm_retraction)),
    /// so without this an aged-out marker would go on refusing until the
    /// process restarts. An issuer that resolves to no replica here has
    /// nothing armed; that is not an error.
    pub fn disarm_retraction(&self, issuer: PdnId, author: AuthorId, key: &[u8]) -> Result<()> {
        let Some(doc) = self.registry.data_doc(issuer)? else {
            return Ok(());
        };
        self.access.disarm_retraction(doc.id(), author, key)
    }

    /// Which issuer `namespace` is bound to on this node, if any — the
    /// reverse resolution a verdict consumer needs (verdicts carry the
    /// replica's namespace).
    pub fn issuer_of_namespace(&self, namespace: NamespaceId) -> Result<Option<PdnId>> {
        Ok(self
            .registry
            .binding_of(namespace)?
            .map(|(issuer, _)| issuer))
    }

    /// Create a fresh doc for a device-shared store; the doc joins the
    /// periodic reconcile pass.
    pub(crate) async fn new_doc(&self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.track(&doc, Vec::new(), SyncStrategy::Swarm)?;
        Ok(doc)
    }

    /// Open a device-shared store's doc this node's store already holds,
    /// and enrol it in the periodic reconcile pass — recovery's
    /// counterpart of [`new_doc`](Self::new_doc) and
    /// [`import_doc`](Self::import_doc): no ticket is consumed and nothing
    /// is created, so a namespace the store does not hold is `Ok(None)`,
    /// not a fresh replica. The absence is separated from a store that
    /// could not answer, which stays an error: the two are what a caller
    /// recovering from a durable record has to tell apart, and one of them
    /// is routine.
    pub(crate) async fn open_doc(&self, namespace: NamespaceId) -> Result<Option<Doc>> {
        // The mirror of [`guard_data_import`](Self::guard_data_import),
        // which refuses a data import onto a device-shared replica: this
        // one refuses opening a data replica as a device-shared store.
        // Tracking here is `Swarm`, so without the guard an import that
        // deliberately stays out of the gossip swarm — every grantee one —
        // would be pulled into it by a mistaken open.
        if self.registry.binding_of(namespace)?.is_some() {
            return Err(anyhow::anyhow!(
                "namespace {namespace} is a data replica on this node; \
                 it cannot be opened as a device-shared store"
            ));
        }
        if !self.holds_namespace(namespace).await? {
            return Ok(None);
        }
        let Some(doc) = self
            .docs
            .open(namespace)
            .await
            .with_context(|| format!("namespace {namespace} did not open"))?
        else {
            return Ok(None);
        };
        self.track(&doc, Vec::new(), SyncStrategy::Swarm)?;
        Ok(Some(doc))
    }

    /// Read the replica store, so a caller can tell a store that still
    /// answers from one that does not. The store is one database shared by
    /// every replica this node holds, and a filesystem that filled under
    /// it leaves it refusing every later operation until it is reopened —
    /// a state no in-memory bookkeeping reflects, which is why a health
    /// answer has to come from the store itself.
    ///
    /// The read asks one replica for its sync peers, because that reaches
    /// the store's tables and a store that stopped answering says so
    /// there. Two cheaper-looking reads do not: the namespace listing and
    /// an empty entry query both keep answering long after the database
    /// has refused everything else — the first is served without reaching
    /// the tables, the second finds nothing pending to commit. A node
    /// tracking no replica has no replica state to be broken, and the
    /// listing is then all there is to ask.
    pub async fn check_replica_store(&self) -> Result<()> {
        let doc = {
            let docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
            docs.values().next().map(|tracked| tracked.doc.clone())
        };
        match doc {
            Some(doc) => {
                let _peers = doc.get_sync_peers().await?;
            }
            None => {
                let mut listed = self.docs.list().await?;
                if let Some(entry) = listed.next().await {
                    let _first = entry?;
                }
            }
        }
        Ok(())
    }

    /// Whether the replica store holds `namespace`, answered from its
    /// listing. Opening cannot answer it: the fork reports "no such
    /// namespace" and "the store could not answer" as one error of the
    /// same shape, so a caller reading them apart would be reading error
    /// text.
    async fn holds_namespace(&self, namespace: NamespaceId) -> Result<bool> {
        let mut listed = self.docs.list().await?;
        while let Some(entry) = listed.next().await {
            let (id, _capability) = entry?;
            if id == namespace {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Import a device-shared store's doc from `ticket`; the doc joins the
    /// periodic reconcile pass together with the ticket's contacts, so a
    /// replica whose initial exchange died is re-dialed rather than starved.
    pub(crate) async fn import_doc(&self, ticket: DocTicket) -> Result<Doc> {
        let contacts = ticket.nodes.clone();
        let doc = self.docs.import(ticket).await?;
        self.track(&doc, contacts, SyncStrategy::Swarm)?;
        Ok(doc)
    }

    /// Register `doc` with the periodic reconcile pass. Keyed by namespace,
    /// so a re-import of a replica this node already tracks replaces its
    /// entry rather than accreting a second one with a contradictory
    /// strategy.
    fn track(&self, doc: &Doc, contacts: Vec<EndpointAddr>, strategy: SyncStrategy) -> Result<()> {
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
            },
        );
        Ok(())
    }

    /// Forget a doc: stop reconciling it and drop the replica — the
    /// rollback for a ceremony that must leave nothing behind. Untracks
    /// before dropping, so the reconcile pass never re-dials a dropped
    /// replica. (Data namespaces roll back through
    /// [`forget_namespace`](Self::forget_namespace) instead, which also
    /// unregisters the issuer.)
    pub async fn forget_doc(&self, namespace: NamespaceId) -> Result<()> {
        {
            let mut docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
            docs.remove(&namespace);
        }
        self.docs.drop_doc(namespace).await?;
        Ok(())
    }

    /// Commit the replica store's open write transaction, so what this node
    /// wrote through it is on disk before anything durable points at it.
    /// Writes are batched into one transaction the store commits on its own
    /// schedule, so a replica created a moment ago is not on disk yet; a
    /// read takes a snapshot, and taking one commits the batch first.
    /// Store-wide although it names a namespace: the snapshot covers every
    /// replica, and the namespace only says which open replica the read
    /// addresses. The read matches nothing — the commit is the point.
    pub async fn flush_replicas(&self, namespace: NamespaceId) -> Result<()> {
        let doc = {
            let docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
            docs.get(&namespace)
                .map(|tracked| tracked.doc.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("namespace {namespace} is not tracked on this node")
                })?
        };
        let _committed = doc.get_many(Query::all().limit(0)).await?;
        Ok(())
    }

    /// Handle to the node's blob store, for stores that read entry payloads.
    pub(crate) fn blobs(&self) -> iroh_blobs::api::Store {
        self.blobs.clone()
    }

    /// Share the data namespace of `issuer` as a ticket other nodes can import.
    pub async fn share_ticket(
        &self,
        issuer: PdnId,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc(issuer)?.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// Create a new author keypair on this node — a standalone writer
    /// identity. The node's own stores do not write with these: they share
    /// the one author of [`default_author`](Self::default_author).
    pub async fn create_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_create().await?;
        Ok(author)
    }

    /// The node's one author — the fork's default author, persisted with
    /// the replicas on a directory-configured node, so a restarted node
    /// writes as the author it wrote as before. Every store on the node
    /// writes with it: an author minted per store would make a rewritten
    /// key accumulate one live record per author, and leave a device record
    /// written under one author standing after a withdrawal written under
    /// another.
    pub async fn default_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_default().await?;
        Ok(author)
    }

    /// This node's identifier on the wire — its iroh endpoint id (an ed25519
    /// public key) as a [`NodeId`].
    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(*self.router.endpoint().id().as_bytes())
    }

    /// A narrow handle onto the node's iroh endpoint for the dial side of
    /// extra protocols ([`DialHandle`]). Deliberately not the raw
    /// [`Endpoint`]: the node stays the sole owner of the endpoint's
    /// lifecycle.
    pub fn dial_handle(&self) -> DialHandle {
        DialHandle {
            endpoint: self.router.endpoint().clone(),
        }
    }

    /// Write `payload` at `path` in the data namespace of `issuer`.
    pub async fn write(
        &self,
        issuer: PdnId,
        author: AuthorId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let doc = self.doc(issuer)?;
        doc.set_bytes(author, path.as_str().as_bytes().to_vec(), payload.to_vec())
            .await?;
        Ok(())
    }

    /// Read the latest payload at `path` in the data namespace of `issuer`,
    /// if present.
    ///
    /// Returns `Ok(None)` both when no entry exists and when the entry is
    /// stored but its payload has not been fetched yet: records and blob
    /// content arrive independently, so "stored" precedes "readable" — poll
    /// again for the payload. Reading a grant-imported (`ContactsOnly`)
    /// namespace nudges its filtered reconciliation first (non-blocking):
    /// the answer is served from the local replica at once, and the nudge
    /// pulls fresh entries for the next read.
    pub async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>> {
        self.nudge_scoped(issuer);
        let doc = self.doc(issuer)?;
        read_payload(&doc, &self.blobs, path.as_str().as_bytes()).await
    }

    /// Fire-and-forget a filtered reconciliation of a `ContactsOnly`
    /// (grant-imported) namespace before serving a read or list. No-op for
    /// swarm-synced bindings and unknown issuers; failures are the
    /// reconcile pass's to retry. Debounced to one in-flight attempt per
    /// namespace — every read and list fires this, and without the latch a
    /// tight poll loop piles up tasks against one replica; cleared when the
    /// attempt finishes, success or not.
    fn nudge_scoped(&self, issuer: PdnId) {
        let Ok(Some(binding)) = self.registry.binding(issuer) else {
            return;
        };
        let namespace = binding.doc.id();
        let Ok(docs) = self.tracked_docs.lock() else {
            return;
        };
        let Some(tracked) = docs.get(&namespace) else {
            return;
        };
        if tracked.strategy != SyncStrategy::ContactsOnly {
            return;
        }
        let doc = tracked.doc.clone();
        let contacts = tracked.contacts.clone();
        drop(docs);
        {
            let Ok(mut in_flight) = self.nudges_in_flight.lock() else {
                return;
            };
            if !in_flight.insert(namespace) {
                return;
            }
        }
        let latch = Arc::clone(&self.nudges_in_flight);
        let _detached = tokio::spawn(async move {
            let _ = doc.start_sync_scoped(contacts).await;
            if let Ok(mut in_flight) = latch.lock() {
                in_flight.remove(&namespace);
            }
        });
    }

    /// List entry metadata in the data namespace of `issuer` — no payload
    /// bytes — optionally narrowed to entries whose path starts with
    /// `path_prefix`, matching whole components (`contacts` matches
    /// `contacts/a` but not `contactsx/c`).
    ///
    /// Record-level: an entry lists once its record is stored, whether or
    /// not its payload has been fetched yet. Deleted entries (tombstones)
    /// do not list.
    pub async fn list(
        &self,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>> {
        self.nudge_scoped(issuer);
        let doc = self.doc(issuer)?;
        // Byte-prefix query as the coarse cut (a component prefix is always
        // a byte prefix); exact component semantics checked per entry below.
        let query = Query::single_latest_per_key();
        let query = match path_prefix {
            Some(prefix) => query.key_prefix(prefix.as_str().as_bytes()),
            None => query,
        };
        let mut stream = std::pin::pin!(doc.get_many(query).await?);
        let mut entries = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            // Keys that don't parse as entry paths are not data-layer
            // entries; skip them, as the store listings do for foreign keys.
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

    /// Shut the node down, closing the endpoint and all protocols. Takes
    /// `&self`: no exclusive ownership is required, and a repeat call is a
    /// no-op (the reconcile-stop send only fires once; the router's own
    /// shutdown is already idempotent).
    pub async fn shutdown(&self) -> Result<()> {
        // Stop the reconcile pass first so it does not race the docs
        // engine's shutdown with fresh sync requests.
        if let Some(stop) = self
            .reconciler_stop
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        {
            let _ = stop.send(());
        }
        self.router.shutdown().await?;
        // The blob store is shut down explicitly rather than left to its
        // last handle's drop: on a directory-configured node it holds its
        // database open, and a node respawned on the same directory would
        // meet its predecessor's lock. Best-effort, keeping the repeat call
        // a no-op: a store already shut down answers with an error, not
        // with a hang.
        let _ = self.blobs.shutdown().await;
        // The directory lock leaves with the stores, not with this value's
        // drop: a detached task holding the node alive a moment longer —
        // an armer mid-sweep — must not make a spawn on the same directory
        // read as a second running node. Best-effort for the same
        // idempotence reason as above.
        if let Some(lock) = &self.directory_lock {
            let _ = lock.unlock();
        }
        Ok(())
    }

    fn doc(&self, issuer: PdnId) -> Result<Doc> {
        self.registry
            .data_doc(issuer)?
            .ok_or_else(|| UnknownIssuer { issuer }.into())
    }
}

/// The layout of a configured storage directory
/// ([`StorageConfig::Directory`]). What a person finds on a volume: `docs/`
/// — the fork's replica store (`docs.redb`) and its persisted author
/// (`default-author`); `blobs/` — payload bytes; `node.key` — the
/// endpoint's secret key, hex-encoded; `lock` — the running node's
/// exclusive hold on the directory, content-free.
const DOCS_DIR: &str = "docs";
/// See [`DOCS_DIR`].
const BLOBS_DIR: &str = "blobs";
/// See [`DOCS_DIR`].
const NODE_KEY_FILE: &str = "node.key";
/// See [`DOCS_DIR`].
const LOCK_FILE: &str = "lock";

/// Owner-only permissions for the storage directory: the replica store
/// inside holds namespace secrets and the blob store payload bytes in the
/// clear, so the boundary sits on the directory rather than on the key file
/// alone.
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
/// Owner-only permissions for the key file.
#[cfg(unix)]
const KEY_MODE: u32 = 0o600;

/// Create the storage directory with owner-only permissions when it is
/// absent, verifying the permissions of what was created; a directory that
/// already exists — a mounted volume, a re-open — is taken as the caller
/// gave it. The `docs/` and `blobs/` subdirectories are created either way:
/// the stores expect their paths to exist, and the boundary sits on the
/// directory itself.
fn provision_directory(directory: &std::path::Path) -> Result<()> {
    if !directory.exists() {
        create_owner_only_dir(directory)?;
    }
    for sub in [DOCS_DIR, BLOBS_DIR] {
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
    // Checked, not assumed: the process umask can strip permission bits at
    // creation, and a directory wider than owner-only exposes namespace
    // secrets.
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

/// Take the node's exclusive hold on the directory: an advisory lock on the
/// `lock` file, held for the node's lifetime and released with the process.
/// The stores below take exclusive locks of their own, but this one comes
/// first, for two reasons: the refusal names the directory and its cause
/// rather than arriving as a lock error from three layers down that reads
/// as corruption — and the blob store's open on a database another node
/// holds waits instead of failing, so a start that reached it would hang
/// rather than refuse.
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

/// Read the endpoint's secret key from `node.key`, or generate and store
/// one on the first start. A key file that is present but cannot be read or
/// parsed stops the start with an error naming it — never a regenerated
/// key: regenerating would silently change the node id, which is the exact
/// failure the stored key exists to prevent.
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
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Generate a fresh key and commit it to `path` — written beside and linked
/// into place, so no half-written key can exist, and linked exclusively, so
/// two starts racing on one directory cannot mint different keys: the loser
/// reads the winner's file instead.
#[cfg(unix)]
fn generate_node_key(directory: &std::path::Path, path: &std::path::Path) -> Result<SecretKey> {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let fresh = SecretKey::generate();
    let encoded = encode_hex(&fresh.to_bytes());
    let staged = directory.join(format!("{NODE_KEY_FILE}.tmp"));
    // One staging name, cleared before use: this runs under the directory's
    // exclusive lock, so no other node is staging here, and a leftover from
    // a start interrupted mid-write must not be what stops every later one.
    // Removed rather than truncated so the mode below is the file's own and
    // not a leftover's.
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
    // Checked, not assumed — the same reason as the directory's own check.
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

/// Name the directory in a failed store open, and tell the one failure that
/// is not corruption apart from the rest: a replica store whose exclusive
/// lock another running node holds surfaces as [`DirectoryHeld`] rather
/// than as a lock error from three layers down that reads as a corrupt
/// store. The underlying error stays in the chain either way.
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

/// Bind the node's endpoint, with `secret_key` when the node's storage
/// holds one — the node id is then the one it had before — and a fresh key
/// otherwise. If `PDN_BIND_ADDR` holds an IP address the endpoint binds
/// that address with an ephemeral port; unset, it binds all interfaces.
/// Scenario tests bind `127.0.0.1` (the just recipes set it) to keep test
/// traffic on loopback; production spawns leave it unset.
async fn bind_endpoint(secret_key: Option<SecretKey>) -> Result<Endpoint> {
    let builder = Endpoint::builder(presets::Minimal);
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

/// Wait until the freshly bound endpoint reports a dialable address. No
/// timeout: an endpoint with no address cannot be dialed, and the local
/// socket's address appears as soon as any transport address is published.
async fn wait_until_dialable(endpoint: &Endpoint) {
    while endpoint.watch_addr().get().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Read the latest entry at `key` and its payload, if the record is here and
/// its blob has arrived.
///
/// `Ok(None)` covers both "no such entry" and "record stored, payload not
/// yet fetched": records and blob content travel independently, so "stored"
/// precedes "readable" and consumers poll. Every payload-waiting read in
/// this layer goes through here; what a caller makes of the bytes is the
/// caller's own.
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

/// The periodic reconcile pass: every `interval`, re-request a sync for
/// each tracked doc with its import-time contacts (the engine unions them
/// with the peers it recorded as useful). A request against a pair whose
/// sync is running is dropped by the engine's session state; a failed
/// request is retried by the next pass. Ends when `stop` is sent
/// ([`SyncNode::shutdown`]) or its sender is dropped with the node.
async fn reconcile_pass(
    interval: Duration,
    docs: Arc<Mutex<HashMap<NamespaceId, TrackedDoc>>>,
    mut stop: oneshot::Receiver<()>,
) {
    while tokio::time::timeout(interval, &mut stop).await.is_err() {
        let snapshot: Vec<TrackedDoc> = match docs.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            // A poisoned lock means a tracking write panicked; skip this
            // pass rather than poison the task — the next tick retries.
            Err(_poisoned) => continue,
        };
        for tracked in snapshot {
            // Best-effort: a failed re-request is retried by the next tick.
            // `ContactsOnly` docs re-sync without joining the gossip swarm.
            let _ = match tracked.strategy {
                SyncStrategy::ContactsOnly => tracked.doc.start_sync_scoped(tracked.contacts).await,
                SyncStrategy::Swarm => tracked.doc.start_sync(tracked.contacts).await,
            };
        }
    }
}

/// Parse a stored key back into an [`EntryPath`], if it is one.
fn path_of(key: &[u8]) -> Option<EntryPath> {
    let s = std::str::from_utf8(key).ok()?;
    EntryPath::new(s).ok()
}

/// Whether `path`'s leading components equal `prefix`'s components. Both
/// are validated paths (no empty components, no trailing slash), so a byte
/// prefix plus a component boundary is exactly component semantics.
fn starts_with_components(path: &EntryPath, prefix: &EntryPath) -> bool {
    match path.as_str().strip_prefix(prefix.as_str()) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reclassification of a held replica store, tested directly
    /// because no path reaches it: the directory's own advisory lock is
    /// taken first and refuses the second node before any store opens, so
    /// this branch is a backstop for a directory whose lock does not hold
    /// — and a backstop no scenario can reach is exactly the code that
    /// rots unnoticed when the fork's error shape drifts.
    ///
    /// Both halves are asserted together: the held error becomes
    /// `DirectoryHeld` naming the directory, and an unrelated failure does
    /// not — a reclassification that fired on everything would name the
    /// directory just as well while telling the operator the opposite of
    /// what happened.
    #[test]
    fn a_held_database_is_reclassified_and_nothing_else_is() {
        let directory = std::path::PathBuf::from("/pdn/state");
        let storage = StorageConfig::Directory(directory.clone());

        // Wrapped in a context layer, the way the fork's own chain
        // presents it — the classification reads the chain, not the
        // outermost error.
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

        // A memory node has no directory to name, so nothing is annotated.
        let on_memory = annotate_store_error(anyhow::anyhow!("boom"), &StorageConfig::Memory);
        assert_eq!(format!("{on_memory:#}"), "boom");
    }
}
