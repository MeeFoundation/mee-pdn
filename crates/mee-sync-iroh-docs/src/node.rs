//! The assembled sync stack: endpoint + gossip + blobs + capability-gated
//! docs, addressed in domain terms.

use std::sync::Arc;

use anyhow::{bail, Result};
use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_docs::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc, DocsApi,
    },
    protocol::Docs,
    store::Query,
    AuthorId, DocTicket, ALPN as DOCS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use mee_sync_api::{EntryPath, NamespaceId};

use crate::gate::{self, IngestPolicy};
use crate::registry::{NamespaceIndex, Registry};

/// One running node: iroh endpoint, gossip, in-memory blob store, and the
/// capability-gated docs engine, with replicas addressed by domain
/// [`NamespaceId`]s and entries by [`EntryPath`]s.
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
    /// Spawn the full stack with `policy` installed at the ingest gate.
    pub async fn spawn(policy: impl IngestPolicy) -> Result<Self> {
        let endpoint = Endpoint::bind(presets::Minimal).await?;
        let blobs = MemStore::default();
        let gossip = Gossip::builder().spawn(endpoint.clone());

        let index = NamespaceIndex::default();
        let validator = gate::capability_validator(Arc::new(policy), index.clone());

        let docs = Docs::memory()
            .capability_validator(validator)
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
            registry: Registry::new(index),
        })
    }

    /// Create a fresh doc and bind it to `namespace`.
    pub async fn create_namespace(&mut self, namespace: NamespaceId) -> Result<()> {
        let doc = self.docs.create().await?;
        self.registry.bind(namespace, doc);
        Ok(())
    }

    /// Import a doc shared via `ticket` and bind it to `namespace`.
    ///
    /// Binding teaches the ingest gate which domain namespace — and thus
    /// which issuer — incoming entries of this doc belong to.
    pub async fn import_namespace(
        &mut self,
        namespace: NamespaceId,
        ticket: DocTicket,
    ) -> Result<()> {
        let doc = self.docs.import(ticket).await?;
        self.registry.bind(namespace, doc);
        Ok(())
    }

    /// Share `namespace` as a ticket other nodes can import.
    pub async fn share_ticket(
        &self,
        namespace: &NamespaceId,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc(namespace)?.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// Create a new author keypair on this node.
    pub async fn create_author(&self) -> Result<AuthorId> {
        let author = self.docs.author_create().await?;
        Ok(author)
    }

    /// Write `payload` at `path` in `namespace`.
    pub async fn write(
        &self,
        namespace: &NamespaceId,
        author: AuthorId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let doc = self.doc(namespace)?;
        doc.set_bytes(author, path.as_str().as_bytes().to_vec(), payload.to_vec())
            .await?;
        Ok(())
    }

    /// Read the latest payload at `path` in `namespace`, if present.
    pub async fn read(&self, namespace: &NamespaceId, path: &EntryPath) -> Result<Option<Vec<u8>>> {
        let doc = self.doc(namespace)?;
        let query = Query::single_latest_per_key().key_exact(path.as_str().as_bytes());
        match doc.get_one(query).await? {
            Some(entry) => {
                let bytes = self.blobs.get_bytes(entry.content_hash()).await?;
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

    fn doc(&self, namespace: &NamespaceId) -> Result<&Doc> {
        match self.registry.doc(namespace) {
            Some(doc) => Ok(doc),
            None => bail!("namespace not bound on this node: {namespace:?}"),
        }
    }
}
