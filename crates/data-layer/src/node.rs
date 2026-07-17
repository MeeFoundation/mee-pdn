//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. Externally supplied protocols — pdn-node's pairing and
//! linking dialogues (ADR-0011, ADR-0012) — register on the same endpoint
//! at spawn, next to the built-in stack, and a narrow dial handle onto that
//! endpoint is exposed for their dial sides. The registration point stays
//! protocol-agnostic (the ceremonies' semantics belong in pdn-node, not
//! here), not a general protocol-extension facility.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_lite::{FutureExt, StreamExt};
use iroh::{
    endpoint::{presets, Connection},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId,
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

use crate::registry::Registry;

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

/// A protocol supplied to [`SyncNode::spawn_with_protocols`] — pdn-node's
/// pairing and linking handlers (ADR-0011, ADR-0012): the ALPN it answers
/// under, and the handler dispatched for connections arriving on it. A
/// plain pair because data-layer owns none of the ceremonies' semantics,
/// not a general extension type.
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
/// cannot escape into iroh's router accept loop, where iroh treats a
/// panicking handler task as fatal and tears the whole node down — gossip,
/// docs, and blobs with it. A caught panic drops just that one connection
/// (iroh logs the returned error) and the node keeps serving.
///
/// Containment is a promise about the node, not about what the dialer's
/// stream read returns. The panic unwinds the handler's own future first,
/// dropping its `SendStream` before the connection, and dropping one
/// implicitly finishes the stream — so a FIN is queued before the
/// connection's close, and a dialer may observe a clean empty
/// end-of-stream rather than an error, whichever reaches it first. Nothing
/// here can change that: the guard only runs once the unwind is over.
///
/// This does not survive a `panic = "abort"` build; the contract on
/// [`SyncNode::spawn_with_protocols`] still asks handlers not to panic.
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
/// this node holds open.
///
/// Gossip broadcasts are best-effort: one fired into a still-forming or
/// quietly degraded neighborhood is lost, and the rescue triggers
/// (neighbor-up, sync reports) ride that same gossip — without this pass a
/// late write can starve until some unrelated contact. Each pass re-dials a
/// doc's import-time contacts plus the peers the engine has recorded as
/// useful for it; the import contacts matter because the engine records a
/// peer only after one *successful* exchange — if a doc's initial exchange
/// dies, there is no recorded peer to re-dial, and without the import
/// contacts the replica would starve permanently on one transient failure.
/// Pairs with a sync already running are skipped by the engine's session
/// state. Sized for interactive replicas with a handful of peers;
/// reconcile-on-access replaces it when subset-rbsr lands.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// One running node: iroh endpoint, gossip, in-memory blob store, and the
/// docs engine, with data replicas addressed by their issuer [`PdnId`] and
/// entries by [`EntryPath`]s. One node hosts the store sets of any number of
/// identities. Every doc the node opens joins a periodic reconcile pass
/// ([`RECONCILE_INTERVAL`]) that keeps replicas converging when gossip
/// loses a broadcast. Externally supplied protocols — pairing and linking
/// (ADR-0011, ADR-0012) — join the same endpoint at spawn
/// ([`SyncNode::spawn_with_protocols`]); their dial sides and the node's
/// own address are reached through [`SyncNode::dial_handle`].
///
/// No ingest filter is installed: the fork's `validate_entry` hook
/// (ADR-0008) stays available but unused, and whatever a replica syncs from
/// a peer holding its ticket is persisted. Until subset-rbsr and `UWill`
/// land, access to a replica is bounded by possession of its ticket.
///
/// Storage is in-memory for now (experiment stage); a persistent variant
/// can be added without changing this surface.
#[derive(Debug)]
pub struct SyncNode {
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: DocsApi,
    registry: Registry,
    /// Every doc handle this node opened — data namespaces and device-shared
    /// stores alike — for the periodic reconcile pass.
    tracked_docs: Arc<Mutex<Vec<TrackedDoc>>>,
    /// Ends the periodic reconcile pass when dropped — with the node — or by
    /// the explicit send in [`SyncNode::shutdown`].
    reconciler_stop: oneshot::Sender<()>,
}

/// One doc under the periodic reconcile pass: the handle, plus the contacts
/// its import ticket carried (empty for docs created here). The contacts
/// keep a replica whose initial exchange died reachable — the engine
/// records a peer as useful only after one successful exchange, so they are
/// the only recovery path until that first success.
#[derive(Debug, Clone)]
struct TrackedDoc {
    doc: Doc,
    contacts: Vec<EndpointAddr>,
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
    displaced: Option<Doc>,
}

/// The dial side of a node's protocols, handed out by
/// [`SyncNode::dial_handle`]. Wraps the node's iroh endpoint but exposes
/// only what a dial needs — connect out, read the node's own address and
/// wire id — never the endpoint's lifecycle. Closing or reconfiguring the
/// socket stays the node's own job ([`SyncNode::shutdown`]); this handle
/// makes that a matter of construction, not of trust.
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
    /// Spawn the full stack with no externally supplied protocols.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_with_protocols(Vec::new()).await
    }

    /// Spawn the full stack, serving `extra_protocols` on the same endpoint
    /// next to the built-in ones (the ceremony slot: pdn-node's pairing and
    /// linking dialogues register here — ADR-0011, ADR-0012). A connection
    /// arriving under a registered extra ALPN
    /// is dispatched to its handler as a raw bidirectional connection — not
    /// a document-sync session. ALPNs must be unique across
    /// [`BUILT_IN_ALPNS`] and the extras; a collision fails the spawn with
    /// [`AlpnTaken`] before anything binds.
    ///
    /// A handler's `accept` should not panic — return `Err(AcceptError)` for
    /// failure instead. A panic is contained (caught per connection, so it
    /// drops only that connection and never the node's built-in sync), but
    /// it is not a supported control-flow path, and a `panic = "abort"`
    /// build would still abort the process.
    pub async fn spawn_with_protocols(extra_protocols: Vec<ExtraProtocol>) -> Result<Self> {
        // Checked before the endpoint binds: nothing to unwind on failure.
        // A collision must never register — an extra silently replacing a
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

        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let docs_api = docs.api().clone();
        let blobs_store: iroh_blobs::api::Store = (*blobs).clone();
        let mut router = Router::builder(endpoint)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
            .accept(GOSSIP_ALPN, gossip)
            .accept(DOCS_ALPN, docs);
        // Each extra is wrapped so a panic in its handler cannot escape into
        // iroh's accept loop, where a panicking task is fatal to the whole
        // node (`PanicGuarded`).
        for (alpn, handler) in extra_protocols {
            router = router.accept(alpn, PanicGuarded { inner: handler });
        }
        let router = router.spawn();
        let tracked_docs: Arc<Mutex<Vec<TrackedDoc>>> = Arc::default();
        let (reconciler_stop, stop) = oneshot::channel();
        let _detached = tokio::spawn(reconcile_pass(Arc::clone(&tracked_docs), stop));
        Ok(Self {
            router,
            blobs: blobs_store,
            docs: docs_api,
            registry: Registry::default(),
            tracked_docs,
            reconciler_stop,
        })
    }

    /// Create a fresh doc and register it as the data namespace of `issuer`.
    pub async fn create_namespace(&self, issuer: PdnId) -> Result<()> {
        let doc = self.new_doc().await?;
        // A registration cannot already exist: `issuer` is minted fresh from
        // the operating system's generator by the caller that provisions it,
        // so there is nothing here to displace or restore.
        let _displaced = self.registry.register_data(issuer, doc)?;
        Ok(())
    }

    /// Import a doc shared via `ticket` and register it as the data namespace
    /// of `issuer`, so reads and writes under `issuer` resolve to it.
    ///
    /// Returns what the import did, so an act that must be undoable can undo
    /// exactly that through [`undo_import_namespace`](Self::undo_import_namespace).
    /// An issuer can already be bound when the import happens — a namespace
    /// reached through a peer's grant registers here too — and replacing that
    /// binding is not this call's to keep: the token carries what was
    /// displaced.
    pub async fn import_namespace(
        &self,
        issuer: PdnId,
        ticket: DocTicket,
    ) -> Result<NamespaceImport> {
        let doc = self.import_doc(ticket).await?;
        let imported = doc.id();
        let displaced = self.registry.register_data(issuer, doc)?;
        Ok(NamespaceImport {
            issuer,
            imported,
            displaced,
        })
    }

    /// Undo an [`import_namespace`](Self::import_namespace): leave exactly the
    /// state that preceded it, touching nothing the import did not touch.
    ///
    /// Two shapes, because an import either bound a free issuer or replaced a
    /// binding it found. A free issuer is unbound again and the replica this
    /// import brought up is dropped — the plain rollback. A replaced binding
    /// is put back, and the imported replica is dropped **only** when it is a
    /// different one: with one namespace per issuer (ADR-0009) an import
    /// under an already-bound issuer normally resolves to the very replica
    /// that binding names, and dropping it would destroy the data the restore
    /// exists to preserve — `drop_doc` is permanent.
    pub async fn undo_import_namespace(&self, import: NamespaceImport) -> Result<()> {
        let NamespaceImport {
            issuer,
            imported,
            displaced,
        } = import;
        let Some(previous) = displaced else {
            return self.forget_namespace(issuer).await;
        };
        let previous_namespace = previous.id();
        let _replaced = self.registry.register_data(issuer, previous)?;
        if imported != previous_namespace {
            self.forget_doc(imported).await?;
        }
        Ok(())
    }

    /// Forget the data namespace of `issuer` — the counterpart of
    /// [`import_namespace`](Self::import_namespace): stop reconciling the
    /// replica, drop it, and remove the issuer's registration, as one act.
    /// Operations addressed to `issuer` afterwards fail with
    /// [`UnknownIssuer`], exactly as before the import — the rollback path
    /// for an import that must not survive the act that made it (device
    /// linking's failed `link`). Dropping the replica without unregistering
    /// is deliberately not offered: the issuer would keep resolving to a
    /// dropped replica, and its operations would fail as storage errors
    /// instead of the distinguishable refusal.
    pub async fn forget_namespace(&self, issuer: PdnId) -> Result<()> {
        let doc = self
            .registry
            .unregister_data(issuer)?
            .ok_or(UnknownIssuer { issuer })?;
        self.forget_doc(doc.id()).await
    }

    /// Create a fresh doc for a device-shared store. Returns the backing doc
    /// for the store handle ([`PrivateMetadataStore`](crate::PrivateMetadataStore),
    /// [`ConnectionMetadataStore`](crate::ConnectionMetadataStore)) to hold.
    /// The doc joins the periodic reconcile pass.
    pub(crate) async fn new_doc(&self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.track(&doc, Vec::new())?;
        Ok(doc)
    }

    /// Import a device-shared store's doc from `ticket` (device linking,
    /// connection metadata). Returns the backing doc for the store handle to
    /// hold. The doc joins the periodic reconcile pass together with the
    /// ticket's contacts, so a replica whose initial exchange died is
    /// re-dialed rather than starved.
    pub(crate) async fn import_doc(&self, ticket: DocTicket) -> Result<Doc> {
        let contacts = ticket.nodes.clone();
        let doc = self.docs.import(ticket).await?;
        self.track(&doc, contacts)?;
        Ok(doc)
    }

    /// Register `doc` with the periodic reconcile pass, re-contactable at
    /// `contacts` (its import ticket's peers; empty for docs created here).
    fn track(&self, doc: &Doc, contacts: Vec<EndpointAddr>) -> Result<()> {
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        docs.push(TrackedDoc {
            doc: doc.clone(),
            contacts,
        });
        Ok(())
    }

    /// Forget a doc: stop reconciling it and drop the replica. The rollback
    /// for a ceremony that must leave nothing behind — a metadata replica
    /// minted for an establishment that then failed before it committed, or
    /// a directory imported by a link that then could not catch up.
    /// Untracks before dropping, so the reconcile pass never re-dials a
    /// dropped replica. (Data namespaces roll back through
    /// [`forget_namespace`](Self::forget_namespace) instead, which also
    /// unregisters the issuer.)
    pub async fn forget_doc(&self, namespace: NamespaceId) -> Result<()> {
        {
            let mut docs = self
                .tracked_docs
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
            docs.retain(|tracked| tracked.doc.id() != namespace);
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
    /// public key) as a [`NodeId`]. A device uses it to register itself in
    /// its identity's device set.
    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(*self.router.endpoint().id().as_bytes())
    }

    /// A narrow handle onto the node's iroh endpoint for the dial side of
    /// extra protocols ([`DialHandle`]): connect out under a chosen ALPN,
    /// and read this node's own address and wire id. Deliberately not the
    /// raw [`Endpoint`] — the node stays the sole owner of the endpoint's
    /// lifecycle, so closing or reconfiguring it is [`SyncNode::shutdown`]'s
    /// job and is not reachable from here.
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

    /// Read the latest payload at `path` in `namespace`, if present.
    ///
    /// Returns `Ok(None)` both when no entry exists and when the entry is
    /// already stored but its payload has not been fetched yet: entry
    /// records and blob content arrive independently (sync inserts the
    /// record, the downloader fetches the bytes), so "stored" precedes
    /// "readable". Poll again for the payload to become available.
    pub async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>> {
        let doc = self.doc(issuer)?;
        read_payload(&doc, &self.blobs, path.as_str().as_bytes()).await
    }

    /// List entry metadata in the data namespace of `issuer` — no payload
    /// bytes — optionally narrowed to entries whose path starts with
    /// `path_prefix`, matching whole components (`contacts` matches
    /// `contacts/a` but not `contactsx/c`).
    ///
    /// Record-level, consistent with record-first reads: an entry lists
    /// once its record is stored, whether or not its payload has been
    /// fetched yet. Deleted entries (tombstones) do not list. Shaped after
    /// [`DataLayer::list_entries`](crate::DataLayer::list_entries), so the
    /// runtime's later switch onto the trait stays mechanical.
    pub async fn list(
        &self,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>> {
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

/// Bind the node's endpoint: all interfaces by default; loopback only when
/// the `PDN_BIND_LOOPBACK` environment variable is `1`.
///
/// The loopback mode exists for in-process scenario tests (the just
/// recipes set the variable): with it the endpoint binds — and therefore
/// advertises — `127.0.0.1`/`[::1]`, so test traffic never crosses a host
/// firewall. Without it, nodes on one machine reach each other through the
/// host's LAN address, where a host firewall can delay the first datagrams
/// of every fresh flow for seconds to minutes (observed on macOS) and
/// starve one-shot dials. Production spawns leave the variable unset.
async fn bind_endpoint() -> Result<Endpoint> {
    let builder = Endpoint::builder(presets::Minimal);
    let builder = if std::env::var("PDN_BIND_LOOPBACK").is_ok_and(|v| v == "1") {
        builder.bind_addr("127.0.0.1:0")?.bind_addr("[::1]:0")?
    } else {
        builder
    };
    Ok(builder.bind().await?)
}

/// Read the latest entry at `key` and its payload, if the record is here and
/// its blob has arrived.
///
/// `Ok(None)` covers both "no such entry" and "the entry is stored but its
/// payload has not been fetched yet": entry records and blob content travel
/// independently — sync inserts the record, the downloader fetches the bytes
/// — so "stored" precedes "readable", and consumers poll. Every
/// payload-waiting read in this layer goes through here, so that rule lives
/// in one place; what a caller makes of the bytes — and of bytes it cannot
/// decode — is the caller's own, and the only thing left at its call site.
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

/// The periodic reconcile pass: every [`RECONCILE_INTERVAL`], re-request a
/// sync for each tracked doc with its import-time contacts. The engine
/// unions those with the peers it recorded as useful for the doc; a request
/// against a pair whose sync is already running is dropped by the engine's
/// session state, and a failed request is simply retried by the next pass.
/// The import contacts are what rescue a replica whose initial exchange
/// died — the engine records peers only on a successful exchange, so until
/// the first success there is nothing recorded to re-dial.
///
/// Paced by timing out the wait on `stop`: the pass ends when the stop is
/// sent ([`SyncNode::shutdown`]) or its sender is dropped with the node.
async fn reconcile_pass(docs: Arc<Mutex<Vec<TrackedDoc>>>, mut stop: oneshot::Receiver<()>) {
    while tokio::time::timeout(RECONCILE_INTERVAL, &mut stop)
        .await
        .is_err()
    {
        let snapshot: Vec<TrackedDoc> = match docs.lock() {
            Ok(guard) => guard.clone(),
            // A poisoned lock means a tracking write panicked; skip this
            // pass rather than poison the task — the next tick retries.
            Err(_poisoned) => continue,
        };
        for tracked in snapshot {
            // Best-effort by design: a failed re-request is not an error of
            // the pass, the next tick retries it.
            let _ = tracked.doc.start_sync(tracked.contacts).await;
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
