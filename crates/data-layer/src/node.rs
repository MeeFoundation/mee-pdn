//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. Externally supplied protocols — pdn-node's pairing and
//! linking dialogues (ADR-0011, ADR-0012) — register on the same endpoint at
//! spawn; a narrow dial handle serves their dial sides. The registration
//! point is protocol-agnostic: the ceremonies' semantics live in pdn-node.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_lite::{FutureExt, StreamExt};
use iroh::{
    endpoint::{presets, Connection},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, Watcher as _,
};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
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

use crate::access::{session_access_provider, AccessBook};
use crate::connection_metadata::ConnectionMetadataStore;
use crate::private_metadata::PrivateMetadataStore;
use crate::registry::{Registry, ServingPosture};

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

/// A protocol supplied to [`SyncNode::spawn_with_protocols`]: the ALPN it
/// answers under, and the handler dispatched for connections arriving on it.
pub type ExtraProtocol = (Vec<u8>, Box<dyn DynProtocolHandler>);

/// The ALPNs of the built-in protocols — blob transfer, gossip, document
/// sync. Reserved: an externally supplied protocol claiming one of these is
/// refused at spawn with [`AlpnTaken`].
pub const BUILT_IN_ALPNS: [&[u8]; 3] = [BLOBS_ALPN, GOSSIP_ALPN, DOCS_ALPN];

/// A spawn was handed an extra protocol whose ALPN is already taken — by a
/// built-in protocol ([`BUILT_IN_ALPNS`]) or by another extra in the same
/// call. Downcast from the `anyhow::Error` of
/// [`SyncNode::spawn_with_protocols`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("protocol ALPN already taken: {}", String::from_utf8_lossy(.alpn))]
pub struct AlpnTaken {
    /// The colliding ALPN.
    pub alpn: Vec<u8>,
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

/// Spawn-time tuning of the node stack ([`SyncNode::spawn_with`]).
/// `Default` is the production posture.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// How often the periodic reconcile pass re-requests a sync for every
    /// doc this node holds open (default [`RECONCILE_INTERVAL`]).
    pub reconcile_interval: Duration,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            reconcile_interval: RECONCILE_INTERVAL,
        }
    }
}

/// One running node: iroh endpoint, gossip, in-memory blob store, and the
/// docs engine, with data replicas addressed by their issuer [`PdnId`] and
/// entries by [`EntryPath`]s. One node hosts the store sets of any number of
/// identities. Every doc the node opens joins a periodic reconcile pass
/// ([`SpawnOptions::reconcile_interval`]). Externally supplied protocols
/// join the same endpoint at spawn ([`SyncNode::spawn_with_protocols`]);
/// their dial sides and the node's own address are reached through
/// [`SyncNode::dial_handle`].
///
/// No ingest filter is installed (the fork's `validate_entry` hook,
/// ADR-0008, is unused): whatever a replica syncs from a peer holding its
/// ticket is persisted. Every read session is classified through the node's
/// access book — full for a replica identity's own devices and connection
/// audiences, capability-filtered for granted counterparties, refused as
/// not-hosted otherwise. Enforcement arms per identity by registration
/// ([`SyncNode::host_identity`] / [`SyncNode::host_connection`]) and per
/// replica by [`SyncNode::import_namespace_scoped`]; a node that registers
/// nothing serves any ticket holder the whole replica.
///
/// Storage is in-memory.
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
    /// Ends the periodic reconcile pass when dropped — with the node — or by
    /// the explicit send in [`SyncNode::shutdown`].
    reconciler_stop: oneshot::Sender<()>,
}

/// How a tracked doc re-syncs — independent of the binding's serving
/// posture. `Swarm` joins the replica's gossip swarm — the issuer's own
/// devices and the device-shared stores. `ContactsOnly` re-syncs with the
/// ticket's contacts alone and never joins the swarm — every grantee
/// import, scoped and whole-store alike: gossip broadcasts entries past the
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
    /// Spawn the full stack with no externally supplied protocols and
    /// default [`SpawnOptions`].
    pub async fn spawn() -> Result<Self> {
        Self::spawn_with(Vec::new(), SpawnOptions::default()).await
    }

    /// Spawn the full stack with no externally supplied protocols, tuned by
    /// `options`.
    pub async fn spawn_with_options(options: SpawnOptions) -> Result<Self> {
        Self::spawn_with(Vec::new(), options).await
    }

    /// Spawn the full stack, serving `extra_protocols` on the same endpoint
    /// next to the built-in ones (ADR-0011, ADR-0012). A connection arriving
    /// under a registered extra ALPN is dispatched to its handler as a raw
    /// bidirectional connection — not a document-sync session. ALPNs must be
    /// unique across [`BUILT_IN_ALPNS`] and the extras; a collision fails
    /// the spawn with [`AlpnTaken`] before anything binds.
    ///
    /// A handler's `accept` should return `Err(AcceptError)` rather than
    /// panic: a panic is contained per connection, but a `panic = "abort"`
    /// build still aborts the process.
    pub async fn spawn_with_protocols(extra_protocols: Vec<ExtraProtocol>) -> Result<Self> {
        Self::spawn_with(extra_protocols, SpawnOptions::default()).await
    }

    /// The full-control spawn: extra protocols plus tuning. The other spawn
    /// entries are thin wrappers over this one.
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

        let endpoint = bind_endpoint().await?;
        let blobs = MemStore::default();
        let gossip = Gossip::builder().spawn(endpoint.clone());

        // The access book and registry exist before the engine so the
        // session access provider can close over them; the blob handle is
        // set right after the spawn, before any session can arrive.
        let registry = Arc::new(Registry::default());
        let access = Arc::new(AccessBook::default());
        let docs = Docs::memory()
            .session_access_provider(session_access_provider(
                Arc::clone(&access),
                Arc::clone(&registry),
            ))
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let docs_api = docs.api().clone();
        let blobs_store: iroh_blobs::api::Store = (*blobs).clone();
        access.set_blobs(blobs_store.clone());
        let mut router = Router::builder(endpoint)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
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
            reconciler_stop,
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
        let doc = self.import_doc(ticket).await?;
        let imported = doc.id();
        match self
            .registry
            .register_data(issuer, doc, ServingPosture::Serve)
        {
            Ok(displaced) => Ok(NamespaceImport {
                issuer,
                imported,
                displaced,
                displaced_tracking,
            }),
            Err(err) => {
                // The one-namespace-one-issuer rejection must not clobber
                // the rightful issuer's tracking: `import_doc` replaced it
                // (and joined the swarm) — put it back, membership included.
                if let Some(previous) = displaced_tracking {
                    let _ = self.restore_tracking(previous).await;
                }
                Err(err)
            }
        }
    }

    /// Import a doc shared via `ticket` as a **whole-store grant** of
    /// `issuer`: access arrives through a grant, not through being a device
    /// of the issuer, so — unlike
    /// [`import_namespace`](Self::import_namespace) — this node never
    /// re-serves the replica to third parties and never joins the replica's
    /// gossip swarm. Classified reconciliation with the ticket's contacts is
    /// the only data path; what makes this grant whole-store rather than
    /// scoped lives entirely in the issuer's book, not in the import.
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
    /// — capability-filtered reconciliation with the ticket's contacts is
    /// its only data path — and this node never re-serves the slice to third
    /// parties.
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
    /// [`import_namespace_scoped`](Self::import_namespace_scoped): `Never`
    /// re-serving, `ContactsOnly` sync. The two public names differ only in
    /// what the caller was granted — a distinction the issuer's book
    /// enforces per session.
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
                .register_data(issuer, doc.clone(), ServingPosture::Never)
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
        self.forget_doc(binding.doc.id()).await?;
        let _unregistered = self.registry.unregister_data(issuer)?;
        Ok(())
    }

    /// Create a fresh doc for a device-shared store; the doc joins the
    /// periodic reconcile pass.
    pub(crate) async fn new_doc(&self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.track(&doc, Vec::new(), SyncStrategy::Swarm)?;
        Ok(doc)
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

    /// Create a new author keypair on this node.
    pub async fn create_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_create().await?;
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

    /// Shut the node down, closing the endpoint and all protocols.
    pub async fn shutdown(self) -> Result<()> {
        // Stop the reconcile pass first so it does not race the docs
        // engine's shutdown with fresh sync requests.
        let _ = self.reconciler_stop.send(());
        self.router.shutdown().await?;
        Ok(())
    }

    fn doc(&self, issuer: PdnId) -> Result<Doc> {
        self.registry
            .data_doc(issuer)?
            .ok_or_else(|| UnknownIssuer { issuer }.into())
    }
}

/// Bind the node's endpoint. If `PDN_BIND_ADDR` holds an IP address the
/// endpoint binds that address with an ephemeral port; unset, it binds all
/// interfaces. Scenario tests bind `127.0.0.1` (the just recipes set it) to
/// keep test traffic on loopback; production spawns leave it unset.
async fn bind_endpoint() -> Result<Endpoint> {
    let builder = Endpoint::builder(presets::Minimal);
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
