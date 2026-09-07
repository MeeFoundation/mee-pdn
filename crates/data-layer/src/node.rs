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

/// `issuer` has no created or imported data namespace here. Downcast from
/// the `anyhow::Error` of the issuer-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("data namespace not bound on this node: {issuer}")]
pub struct UnknownIssuer {
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
    /// `docs/` (replica store and persisted author), `blobs/`, `node.key`,
    /// `lock`. Created owner-only when absent: the replica store holds
    /// namespace secrets and the blobs payload bytes in the clear.
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
}

impl SpawnOptions {
    /// In memory, direct paths — what the in-process suites run on.
    pub fn memory() -> Self {
        Self {
            storage: StorageConfig::Memory,
            reconcile_interval: RECONCILE_INTERVAL,
            connectivity: Connectivity::Direct,
        }
    }

    /// Under `directory`, direct paths — what the container stand runs on.
    pub fn on_directory(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            storage: StorageConfig::Directory(directory.into()),
            reconcile_interval: RECONCILE_INTERVAL,
            connectivity: Connectivity::Direct,
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

/// One running node: iroh endpoint, gossip, blob store, and the docs
/// engine, with data replicas addressed by issuer [`PdnId`] and entries by
/// [`EntryPath`]. Every doc the node opens joins the periodic reconcile
/// pass. A node that registers nothing serves — and admits — any ticket
/// holder the whole replica.
#[derive(Debug)]
pub struct SyncNode {
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: DocsApi,
    registry: Arc<Registry>,
    access: Arc<AccessBook>,
    /// Keyed by namespace, so a re-import replaces its entry rather than
    /// accreting a second one.
    tracked_docs: Arc<Mutex<HashMap<NamespaceId, TrackedDoc>>>,
    /// At most one nudge in flight per namespace, so a tight poll loop
    /// cannot pile up attempts against one replica.
    nudges_in_flight: Arc<Mutex<HashSet<NamespaceId>>>,
    retraction: Arc<RetractionTracker>,
    /// Taken once, by the runtime's consumer.
    retraction_verdicts: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RetractionVerdict>>>,
    /// Taken once, so a repeated `shutdown` is a no-op under a shared
    /// reference.
    reconciler_stop: Mutex<Option<oneshot::Sender<()>>>,
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
/// recovery path for a replica whose initial exchange died.
#[derive(Debug, Clone)]
struct TrackedDoc {
    doc: Doc,
    contacts: Vec<EndpointAddr>,
    strategy: SyncStrategy,
}

/// What one [`SyncNode::import_namespace`] did, so that
/// [`SyncNode::undo_import_namespace`] undoes exactly that. Opaque: it holds
/// the fork's replica handle.
#[derive(Debug)]
pub struct NamespaceImport {
    issuer: PdnId,
    imported: NamespaceId,
    /// `None` if the issuer was free.
    displaced: Option<crate::registry::DataBinding>,
    /// The tracking entry the import replaced; the undo puts it back and
    /// re-aligns swarm membership with its strategy.
    displaced_tracking: Option<TrackedDoc>,
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
                // The blob store holds its database open: a retry on the
                // same directory in this process would wait on it rather
                // than be refused.
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
    /// device records decide who is an own device, and its data namespace
    /// serves fail-closed from here on.
    pub fn host_identity(&self, identity: PdnId, directory: &PrivateMetadataStore) -> Result<()> {
        self.access.host_identity(identity, directory.doc_handle())
    }

    /// The rollback counterpart of [`host_identity`](Self::host_identity).
    /// Registered connections are untouched.
    pub fn unhost_identity(&self, identity: PdnId) -> Result<()> {
        self.access.unhost_identity(identity)
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
        self.access
            .host_connection(identity, peer, own.doc_handle(), peer_store.doc_handle())
    }

    /// Create a fresh doc and register it as the data namespace of `issuer`.
    pub async fn create_namespace(&self, issuer: PdnId) -> Result<()> {
        let doc = self.new_doc().await?;
        // `issuer` is minted fresh by the caller: nothing to displace.
        let _displaced = self
            .registry
            .register_data(issuer, doc, ServingPosture::Serve)?;
        Ok(())
    }

    /// The device-replication import: the issuer's own devices bring the
    /// replica up this way, joining its swarm. A namespace reached through a
    /// grant uses [`import_namespace_scoped`](Self::import_namespace_scoped).
    /// Returns what the import did, undoable through
    /// [`undo_import_namespace`](Self::undo_import_namespace). A ticket
    /// naming a tracked but not data-bound replica (a directory, a
    /// connection metadata store) is refused.
    pub async fn import_namespace(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let displaced_tracking = self.guard_data_import(ticket.capability.id())?;
        // Capability, binding, then sync: a session arriving at a namespace
        // the book does not know is classified `Full`.
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
                    // the rightful issuer's tracking.
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

    /// The same grantee import as
    /// [`import_namespace_scoped`](Self::import_namespace_scoped); what the
    /// grant covers lives in the issuer's book, not in the import.
    pub async fn import_namespace_granted(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(issuer, ticket).await
    }

    /// The grantee import: never joins the replica's gossip swarm, and
    /// re-serves it only to the devices of the grant's audience identity per
    /// the locally replicated grant record. Returns what the import did,
    /// undoable through
    /// [`undo_import_namespace`](Self::undo_import_namespace).
    pub async fn import_namespace_scoped(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        self.import_grantee_namespace(issuer, ticket).await
    }

    /// Merge the capability `ticket` carries into a namespace this node
    /// already holds: a grant widened to write names the replica the grantee
    /// bound under the read grant before it, and without the merge the
    /// grantee holds a read replica against a record promising a write. A
    /// capability the store already holds is no change.
    pub async fn merge_data_capability(&self, ticket: &DocTicket) -> Result<()> {
        let _doc = self.docs.import_namespace(ticket.capability.clone()).await?;
        Ok(())
    }

    /// Refuses a ticket naming a tracked but not data-bound replica: honoring
    /// it would downgrade that store's sync strategy — leaving the gossip
    /// swarm, cutting its live path — on the word of whoever minted the
    /// ticket.
    async fn import_grantee_namespace(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let contacts = ticket.nodes.clone();
        let displaced_tracking = self.guard_data_import(ticket.capability.id())?;
        // The capability only — no `start_sync`, which would join the
        // swarm. The binding registers before the first sync, so even that
        // session is judged under the grantee rules.
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
        // A device-replicated import downgraded to a grantee binding leaves
        // the swarm now, so membership cannot outlive the strategy.
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

    /// Replace the reconciliation contacts of `issuer`'s data namespace —
    /// replacement is what lets a withdrawn device stop being dialed.
    /// Refuses with [`UnknownIssuer`] whether the issuer was never bound or
    /// is bound but untracked: silently dropping the set would starve the
    /// replica unattributably.
    pub fn set_namespace_contacts(&self, issuer: PdnId, contacts: Vec<EndpointAddr>) -> Result<()> {
        let doc = self
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.set_doc_contacts(doc.id(), contacts).map_err(|err| {
            match err.downcast_ref::<UntrackedNamespace>() {
                Some(_untracked) => UnknownIssuer { issuer }.into(),
                None => err,
            }
        })
    }

    /// Replace the reconciliation contacts of a device-shared store's doc: a
    /// ticket names only the devices of the side that minted it, so the
    /// caller records the devices it knows hold the replica. Refuses with
    /// [`UntrackedNamespace`] rather than dropping the set silently.
    pub fn set_doc_contacts(
        &self,
        namespace: NamespaceId,
        contacts: Vec<EndpointAddr>,
    ) -> Result<()> {
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

    /// The observation side of
    /// [`set_namespace_contacts`](Self::set_namespace_contacts). Empty when
    /// the issuer resolves to no tracked replica.
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

    /// The docs under the reconcile pass — the only anchor a scenario has
    /// for a cancelled attempt whose replica has no other name.
    #[cfg(feature = "test-util")]
    pub fn tracked_doc_count(&self) -> Result<usize> {
        let docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("tracked docs lock poisoned"))?;
        Ok(docs.len())
    }

    /// Live records at `path` across authors — what every latest-wins read
    /// collapses, so this is the only way to assert one author per node.
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

    /// The tracking entry a data import is about to replace; refuses when
    /// the namespace is tracked but not data-bound (a device-shared store).
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

    /// Put back a displaced tracking entry and re-align swarm membership: a
    /// `ContactsOnly` entry leaves the swarm now (best-effort), a `Swarm`
    /// entry re-joins on the next reconcile pass by itself.
    async fn restore_tracking(&self, tracking: TrackedDoc) -> Result<()> {
        self.track(&tracking.doc, tracking.contacts.clone(), tracking.strategy)?;
        if tracking.strategy == SyncStrategy::ContactsOnly {
            let _ = tracking.doc.leave_gossip().await;
        }
        Ok(())
    }

    /// Leave exactly the state that preceded the import. A replaced binding
    /// is put back, and the imported replica dropped only when it is a
    /// different one: with one namespace per issuer (ADR-0009) an import
    /// under a bound issuer resolves to the very replica the binding names,
    /// and `drop_doc` is permanent.
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
            // Same replica: the restored binding must sync under the entry
            // it was recorded with.
            self.restore_tracking(tracking).await?;
        }
        Ok(())
    }

    /// Stop reconciling `issuer`'s replica, drop it, and unregister the
    /// issuer, as one act — so operations afterwards fail with
    /// [`UnknownIssuer`] rather than as storage errors against a dropped
    /// replica.
    pub async fn forget_namespace(&self, issuer: PdnId) -> Result<()> {
        // Drop first: the reverse order opens a window in which the replica
        // is alive but unknown to the book, and so served whole; a failed
        // drop leaves the registration in place, so a retry still resolves
        // the issuer.
        let binding = self
            .registry
            .binding(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        let namespace = binding.doc.id();
        self.forget_doc(namespace).await?;
        let _unregistered = self.registry.unregister_data(issuer)?;
        self.access.disarm_retractions(namespace)?;
        self.retraction.untrack_namespace(namespace);
        Ok(())
    }

    /// The registration probe for importers that memoize their imports:
    /// each import holds one more open handle on the replica, and the drop
    /// at the end of its life must find exactly one.
    pub fn data_namespace_of(&self, issuer: PdnId) -> Result<Option<NamespaceId>> {
        Ok(self.registry.data_doc(issuer)?.map(|doc| doc.id()))
    }

    /// Record `author` as one of this node's own writers, so the retraction
    /// tracker recognizes its entries.
    pub fn track_writer_author(&self, author: AuthorId) {
        self.retraction.track_author(author);
    }

    /// Set exactly `devices` as the issuer's device set for retraction
    /// verdicts on `issuer`'s granted namespace.
    pub fn track_retraction_peers(&self, issuer: PdnId, devices: Vec<NodeId>) -> Result<()> {
        let doc = self
            .registry
            .data_doc(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.retraction
            .track_namespace(doc.id(), devices.into_iter().collect());
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
        issuer: PdnId,
        author: AuthorId,
        key: &[u8],
        bound: u64,
    ) -> Result<bool> {
        let doc = self.doc(issuer)?;
        doc.retract(author, key.to_vec(), bound).await
    }

    /// The marker's in-memory half: refuse re-ingest of `author`'s entries
    /// at `key` up to `bound`.
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

    /// Whether this node holds exactly the entry `verdict` names — author,
    /// key, timestamp, content hash. A verdict's fields are the refusing
    /// peer's word and retraction is destructive; a version a newer own
    /// write already superseded must not be undone by a rejection still in
    /// flight for it.
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

    /// Take down what a dropped marker armed. An issuer resolving to no
    /// replica has nothing armed; not an error.
    pub fn disarm_retraction(&self, issuer: PdnId, author: AuthorId, key: &[u8]) -> Result<()> {
        let Some(doc) = self.registry.data_doc(issuer)? else {
            return Ok(());
        };
        self.access.disarm_retraction(doc.id(), author, key)
    }

    /// The reverse resolution a verdict consumer needs.
    pub fn issuer_of_namespace(&self, namespace: NamespaceId) -> Result<Option<PdnId>> {
        Ok(self
            .registry
            .binding_of(namespace)?
            .map(|(issuer, _)| issuer))
    }

    /// A fresh doc for a device-shared store, tracked.
    pub(crate) async fn new_doc(&self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.track(&doc, Vec::new(), SyncStrategy::Swarm)?;
        Ok(doc)
    }

    /// Recovery's counterpart of `new_doc` / `import_doc`: a namespace the
    /// store does not hold is `Ok(None)`, kept apart from a store that could
    /// not answer.
    pub(crate) async fn open_doc(&self, namespace: NamespaceId) -> Result<Option<Doc>> {
        // The mirror of `guard_data_import`: tracking here is `Swarm`, so a
        // grantee import opened by mistake would be pulled into the swarm.
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

    /// Read the replica store, to tell a store that still answers from one
    /// a full filesystem left refusing everything. The read asks one
    /// replica for its sync peers because that reaches the tables; the
    /// namespace listing and an empty entry query both keep answering long
    /// after the database has refused everything else.
    pub async fn check_replica_store(&self) -> Result<()> {
        let doc = {
            let docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
            docs.values().next().map(|tracked| tracked.doc.clone())
        };
        if let Some(doc) = doc {
            let _peers = doc.get_sync_peers().await?;
        } else {
            let mut listed = self.docs.list().await?;
            if let Some(entry) = listed.next().await {
                let _first = entry?;
            }
        }
        Ok(())
    }

    /// Answered from the listing: the fork reports "no such namespace" and
    /// "the store could not answer" as one error of the same shape.
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

    /// Import a device-shared store's doc, tracked with the ticket's
    /// contacts.
    pub(crate) async fn import_doc(&self, ticket: DocTicket) -> Result<Doc> {
        let contacts = ticket.nodes.clone();
        let doc = self.docs.import(ticket).await?;
        self.track(&doc, contacts, SyncStrategy::Swarm)?;
        Ok(doc)
    }

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

    /// Untrack and drop a device-shared store's doc. Data namespaces go
    /// through [`forget_namespace`](Self::forget_namespace), which also
    /// unregisters the issuer.
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

    /// Commit the store's open write transaction, so what this node wrote
    /// is on disk before anything durable points at it: a read takes a
    /// snapshot, and taking one commits the batch first. Store-wide although
    /// it names a namespace; the read matches nothing — the commit is the
    /// point.
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

    pub(crate) fn blobs(&self) -> iroh_blobs::api::Store {
        self.blobs.clone()
    }

    pub async fn share_ticket(
        &self,
        issuer: PdnId,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc(issuer)?.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// A standalone author; the node's own stores write with
    /// [`default_author`](Self::default_author) instead.
    pub async fn create_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_create().await?;
        Ok(author)
    }

    /// The node's one author, persisted with the replicas. An author minted
    /// per store or per start would make a rewritten key accumulate one
    /// live record per author, and leave a device record written under one
    /// author standing after a withdrawal written under another.
    pub async fn default_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_default().await?;
        Ok(author)
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

    /// `Ok(None)` both when no entry exists and when its payload has not
    /// been fetched yet — poll again. A grant-imported namespace is nudged
    /// first (non-blocking): the answer comes from the local replica at once.
    pub async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>> {
        self.nudge_scoped(issuer);
        let doc = self.doc(issuer)?;
        read_payload(&doc, &self.blobs, path.as_str().as_bytes()).await
    }

    /// Fire-and-forget a filtered reconciliation of a `ContactsOnly`
    /// namespace; no-op otherwise. Debounced to one attempt in flight per
    /// namespace, or a tight poll loop piles up tasks against one replica.
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

    /// Entry metadata, record-level, optionally narrowed to `path_prefix`
    /// matching whole components (`contacts` matches `contacts/a`, not
    /// `contactsx/c`).
    pub async fn list(
        &self,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>> {
        self.nudge_scoped(issuer);
        let doc = self.doc(issuer)?;
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
    pub fn doc_contacts(&self, namespace: NamespaceId) -> Result<Vec<EndpointAddr>> {
        let docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        Ok(docs
            .get(&namespace)
            .map(|tracked| tracked.contacts.clone())
            .unwrap_or_default())
    }

    /// Idempotent under a shared reference.
    pub async fn shutdown(&self) -> Result<()> {
        // First, so it does not race the docs engine's shutdown with fresh
        // sync requests.
        if let Some(stop) = self
            .reconciler_stop
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        {
            let _ = stop.send(());
        }
        self.router.shutdown().await?;
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
        Ok(())
    }

    fn doc(&self, issuer: PdnId) -> Result<Doc> {
        self.registry
            .data_doc(issuer)?
            .ok_or_else(|| UnknownIssuer { issuer }.into())
    }
}

/// The directory layout: the fork's replica store (`docs.redb`) and its
/// persisted author (`default-author`).
const DOCS_DIR: &str = "docs";
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

/// Every `interval`, re-request a sync for each tracked doc with its
/// contacts (the engine unions in the peers it recorded). A failed request
/// is retried by the next pass. Ends when `stop` is sent or its sender is
/// dropped with the node.
async fn reconcile_pass(
    interval: Duration,
    docs: Arc<Mutex<HashMap<NamespaceId, TrackedDoc>>>,
    mut stop: oneshot::Receiver<()>,
) {
    while tokio::time::timeout(interval, &mut stop).await.is_err() {
        let snapshot: Vec<TrackedDoc> = match docs.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(_poisoned) => continue,
        };
        for tracked in snapshot {
            let _ = match tracked.strategy {
                SyncStrategy::ContactsOnly => tracked.doc.start_sync_scoped(tracked.contacts).await,
                SyncStrategy::Swarm => tracked.doc.start_sync(tracked.contacts).await,
            };
        }
    }
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
