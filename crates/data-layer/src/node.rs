//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_lite::StreamExt;
use iroh::{endpoint::presets, protocol::Router, Endpoint};
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
/// loses a broadcast.
///
/// No ingest filter is installed: the fork's `validate_entry` seam
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

impl SyncNode {
    /// Spawn the full stack.
    pub async fn spawn() -> Result<Self> {
        let endpoint = Endpoint::bind(presets::Minimal).await?;
        let blobs = MemStore::default();
        let gossip = Gossip::builder().spawn(endpoint.clone());

        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let docs_api = docs.api().clone();
        let blobs_store: iroh_blobs::api::Store = (*blobs).clone();
        let router = Router::builder(endpoint)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
            .accept(GOSSIP_ALPN, gossip)
            .accept(DOCS_ALPN, docs)
            .spawn();
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
