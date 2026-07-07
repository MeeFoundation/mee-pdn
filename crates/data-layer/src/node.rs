//! The assembled sync stack: endpoint + gossip + blobs + docs, addressed in
//! domain terms.

use anyhow::Result;
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
use pdn_types::{EntryPath, NodeId, PdnId};

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

/// One running node: iroh endpoint, gossip, in-memory blob store, and the
/// docs engine, with data replicas addressed by their issuer [`PdnId`] and
/// entries by [`EntryPath`]s. One node hosts the store sets of any number of
/// identities.
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
        Ok(Self {
            router,
            blobs: blobs_store,
            docs: docs_api,
            registry: Registry::default(),
        })
    }

    /// Create a fresh doc and register it as the data namespace of `issuer`.
    pub async fn create_namespace(&mut self, issuer: PdnId) -> Result<()> {
        let doc = self.docs.create().await?;
        self.registry.register_data(issuer, doc);
        Ok(())
    }

    /// Import a doc shared via `ticket` and register it as the data namespace
    /// of `issuer`, so reads and writes under `issuer` resolve to it.
    pub async fn import_namespace(&mut self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let doc = self.docs.import(ticket).await?;
        self.registry.register_data(issuer, doc);
        Ok(())
    }

    /// Create a fresh doc for a device-shared store. Returns the backing doc
    /// for the store handle ([`ConnectionsStore`](crate::ConnectionsStore),
    /// [`PrivateMetadataStore`](crate::PrivateMetadataStore)) to hold.
    pub(crate) async fn new_doc(&mut self) -> Result<Doc> {
        let doc = self.docs.create().await?;
        Ok(doc)
    }

    /// Import a device-shared store's doc from `ticket` (device linking).
    /// Returns the backing doc for the store handle to hold.
    pub(crate) async fn import_doc(&mut self, ticket: DocTicket) -> Result<Doc> {
        let doc = self.docs.import(ticket).await?;
        Ok(doc)
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

    /// Shut the node down, closing the endpoint and all protocols.
    pub async fn shutdown(self) -> Result<()> {
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
