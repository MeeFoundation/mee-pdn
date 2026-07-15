//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms. ADR-0011's pairing dialogue registers as a further protocol
//! on the same endpoint at spawn, next to the built-in stack, and a narrow
//! dial handle onto that endpoint is exposed for its dial side. The
//! registration point stays protocol-agnostic (data-layer owns no pairing
//! semantics), not a general protocol-extension facility.

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
    AuthorId, DocTicket, ALPN as DOCS_ALPN,
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

/// A protocol supplied to [`SyncNode::spawn_with_protocols`] — the pairing
/// handler (ADR-0011): the ALPN it answers under, and the handler dispatched
/// for connections arriving on it. A plain pair because data-layer owns no
/// pairing semantics, not a general extension type.
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
/// (iroh logs the returned error) and the node keeps serving. This does not
/// survive a `panic = "abort"` build; the contract on
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
/// late write can starve until some unrelated contact. The engine re-dials
/// the peers it has recorded as useful for each doc, so the pass keeps no
/// peer bookkeeping of its own, and pairs with a sync already running are
/// skipped by the engine's session state. Sized for interactive replicas
/// with a handful of peers; reconcile-on-access replaces it when
/// subset-rbsr lands.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// One running node: iroh endpoint, gossip, in-memory blob store, and the
/// docs engine, with data replicas addressed by their issuer [`PdnId`] and
/// entries by [`EntryPath`]s. One node hosts the store sets of any number of
/// identities. Every doc the node opens joins a periodic reconcile pass
/// ([`RECONCILE_INTERVAL`]) that keeps replicas converging when gossip
/// loses a broadcast. The pairing protocol (ADR-0011) joins the same
/// endpoint at spawn ([`SyncNode::spawn_with_protocols`]); its dial side and
/// the node's own address are reached through [`SyncNode::dial_handle`].
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
    tracked_docs: Arc<Mutex<Vec<Doc>>>,
    /// Ends the periodic reconcile pass when dropped — with the node — or by
    /// the explicit send in [`SyncNode::shutdown`].
    reconciler_stop: oneshot::Sender<()>,
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
    /// next to the built-in ones (ADR-0011's slot: the pairing dialogue
    /// registers here). A connection arriving under a registered extra ALPN
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

        let endpoint = Endpoint::bind(presets::Minimal).await?;
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
        // Each extra is wrapped so a panic in its handler cannot escape into
        // iroh's accept loop, where a panicking task is fatal to the whole
        // node (`PanicGuarded`).
        for (alpn, handler) in extra_protocols {
            router = router.accept(alpn, PanicGuarded { inner: handler });
        }
        let router = router.spawn();
        let tracked_docs: Arc<Mutex<Vec<Doc>>> = Arc::default();
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
    pub async fn create_namespace(&mut self, issuer: PdnId) -> Result<()> {
        let doc = self.new_doc().await?;
        self.registry.register_data(issuer, doc);
        Ok(())
    }

    /// Import a doc shared via `ticket` and register it as the data namespace
    /// of `issuer`, so reads and writes under `issuer` resolve to it.
    pub async fn import_namespace(&mut self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let doc = self.import_doc(ticket).await?;
        self.registry.register_data(issuer, doc);
        Ok(())
    }

    /// Create a fresh doc for a device-shared store. Returns the backing doc
    /// for the store handle ([`ConnectionsStore`](crate::ConnectionsStore),
    /// [`PrivateMetadataStore`](crate::PrivateMetadataStore)) to hold. The
    /// doc joins the periodic reconcile pass.
    pub(crate) async fn new_doc(&mut self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.track(&doc)?;
        Ok(doc)
    }

    /// Import a device-shared store's doc from `ticket` (device linking).
    /// Returns the backing doc for the store handle to hold. The doc joins
    /// the periodic reconcile pass.
    pub(crate) async fn import_doc(&mut self, ticket: DocTicket) -> Result<Doc> {
        let doc = self.docs.import(ticket).await?;
        self.track(&doc)?;
        Ok(doc)
    }

    /// Register `doc` with the periodic reconcile pass.
    fn track(&self, doc: &Doc) -> Result<()> {
        let mut docs = self
            .tracked_docs
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("reconcile tracking lock poisoned"))?;
        docs.push(doc.clone());
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
        let query = Query::single_latest_per_key().key_exact(path.as_str().as_bytes());
        match doc.get_one(query).await? {
            Some(entry) => {
                let hash = entry.content_hash();
                if !self.blobs.has(hash).await? {
                    return Ok(None);
                }
                let bytes = self.blobs.get_bytes(hash).await?;
                Ok(Some(bytes.to_vec()))
            }
            None => Ok(None),
        }
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

    fn doc(&self, issuer: PdnId) -> Result<&Doc> {
        Ok(self
            .registry
            .data_doc(issuer)
            .ok_or(UnknownIssuer { issuer })?)
    }
}

/// The periodic reconcile pass: every [`RECONCILE_INTERVAL`], re-request a
/// sync for each tracked doc. `start_sync` with no explicit peers has the
/// engine re-dial the peers it recorded as useful for that doc; a request
/// against a pair whose sync is already running is dropped by the engine's
/// session state, and a failed request is simply retried by the next pass.
///
/// Paced by timing out the wait on `stop`: the pass ends when the stop is
/// sent ([`SyncNode::shutdown`]) or its sender is dropped with the node.
async fn reconcile_pass(docs: Arc<Mutex<Vec<Doc>>>, mut stop: oneshot::Receiver<()>) {
    while tokio::time::timeout(RECONCILE_INTERVAL, &mut stop)
        .await
        .is_err()
    {
        let snapshot: Vec<Doc> = match docs.lock() {
            Ok(guard) => guard.clone(),
            // A poisoned lock means a tracking write panicked; skip this
            // pass rather than poison the task — the next tick retries.
            Err(_poisoned) => continue,
        };
        for doc in snapshot {
            // Best-effort by design: a failed re-request is not an error of
            // the pass, the next tick retries it.
            let _ = doc.start_sync(Vec::new()).await;
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
