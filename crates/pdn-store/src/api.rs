//! The handles the platform calls the engine through: [`DocsApi`] for a
//! store, [`Doc`] for one of its replicas.

#![allow(missing_docs)]

use std::sync::{Arc, Weak};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh_blobs::Hash;
use n0_future::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    engine::{Engine, LiveEvent},
    store::{OpenError, Query},
    Author, AuthorId, Capability, CapabilityKind, Contact, DocTicket, Entry, Identity, NamespaceId,
    NamespaceSecret, PeerIdBytes,
};

/// Capacity of the channel a listing streams through.
const STREAM_CAP: usize = 64;

/// The engine behind a handle, or the error every call on a handle meets
/// once the last [`Docs`](crate::protocol::Docs) of its engine is gone.
fn engine(engine: &Weak<Engine>) -> Result<Arc<Engine>> {
    engine
        .upgrade()
        .ok_or_else(|| anyhow!("the store has stopped"))
}

/// Hands a payload's temp tag to the payloads the engine holds however a
/// write ends: the store inserts an entry it was sent whether or not its
/// caller still waits, and a collection run that read the replicas before
/// the insert would otherwise sweep the payload.
struct HeldOnDrop<'a> {
    engine: &'a Engine,
    tag: Option<iroh_blobs::api::TempTag>,
}

impl Drop for HeldOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(tag) = self.tag.take() {
            self.engine.hold_payload(tag);
        }
    }
}

/// Options to configure what is included in a [`iroh::EndpointAddr`].
#[derive(
    Copy,
    Clone,
    PartialEq,
    Eq,
    Default,
    Debug,
    derive_more::Display,
    derive_more::FromStr,
    Serialize,
    Deserialize,
)]
pub enum AddrInfoOptions {
    /// Only the Node ID is added.
    ///
    /// This usually means that iroh-dns address lookup is used to find address information.
    #[default]
    Id,
    /// Includes the Node ID and both the relay URL, and the direct addresses.
    RelayAndAddresses,
    /// Includes the Node ID and the relay URL.
    Relay,
    /// Includes the Node ID and the direct addresses.
    Addresses,
}

impl AddrInfoOptions {
    /// Apply the options to the given address.
    pub fn apply(
        &self,
        iroh::EndpointAddr { id, addrs }: &iroh::EndpointAddr,
    ) -> iroh::EndpointAddr {
        match self {
            Self::Id => iroh::EndpointAddr::new(*id),
            Self::Relay => iroh::EndpointAddr {
                id: *id,
                addrs: addrs
                    .iter()
                    .filter(|addr| matches!(addr, iroh::TransportAddr::Relay(_)))
                    .cloned()
                    .collect(),
            },
            Self::Addresses => iroh::EndpointAddr {
                id: *id,
                addrs: addrs
                    .iter()
                    .filter(|addr| matches!(addr, iroh::TransportAddr::Ip(_)))
                    .cloned()
                    .collect(),
            },
            Self::RelayAndAddresses => iroh::EndpointAddr {
                id: *id,
                addrs: addrs.clone(),
            },
        }
    }
}

/// API wrapper for the docs service
#[derive(Debug, Clone)]
pub struct DocsApi {
    engine: Weak<Engine>,
}

impl DocsApi {
    /// A handle on `engine` that leaves it to whoever holds it strongly.
    pub(crate) fn new(engine: &Arc<Engine>) -> Self {
        Self {
            engine: Arc::downgrade(engine),
        }
    }

    fn engine(&self) -> Result<Arc<Engine>> {
        engine(&self.engine)
    }

    fn doc(&self, namespace_id: NamespaceId) -> Doc {
        Doc::new(self.engine.clone(), namespace_id)
    }

    /// Creates a new document author.
    ///
    /// You likely want to save the returned [`AuthorId`] somewhere so that you can use this author
    /// again.
    ///
    /// If you need only a single author, use [`Self::author_default`].
    pub async fn author_create(&self) -> Result<AuthorId> {
        let author = Author::new(&mut rand::rng());
        self.engine()?.sync.import_author(author).await
    }

    /// Returns the default document author of this node.
    ///
    /// On persistent nodes, the author is created on first start and its public key is saved
    /// in the data directory.
    pub async fn author_default(&self) -> Result<AuthorId> {
        Ok(self.engine()?.default_author.get())
    }

    /// Creates a new document.
    pub async fn create(&self) -> Result<Doc> {
        let engine = self.engine()?;
        let namespace = NamespaceSecret::new(&mut rand::rng());
        let id = namespace.id();
        engine.sync.import_namespace(namespace.into()).await?;
        engine.sync.open(id, Default::default()).await?;
        Ok(self.doc(id))
    }

    /// Deletes a document from the local node.
    ///
    /// This is a destructive operation. Both the document secret key and all entries in the
    /// document will be permanently deleted from the node's storage. Content blobs will be deleted
    /// through garbage collection unless they are referenced from another document or tag.
    pub async fn drop_doc(&self, doc_id: NamespaceId) -> Result<()> {
        self.engine()?.drop_replica(doc_id).await
    }

    /// Imports a document from a namespace capability.
    ///
    /// This does not start sync automatically. Use [`Doc::start_sync`] to start sync.
    pub async fn import_namespace(&self, capability: Capability) -> Result<Doc> {
        let engine = self.engine()?;
        let doc_id = engine.sync.import_namespace(capability).await?;
        engine.sync.open(doc_id, Default::default()).await?;
        Ok(self.doc(doc_id))
    }

    /// Lists all documents.
    pub async fn list(
        &self,
    ) -> Result<impl Stream<Item = Result<(NamespaceId, CapabilityKind)>> + Unpin + Send + 'static>
    {
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CAP);
        self.engine()?.sync.list_replicas(tx).await?;
        Ok(ReceiverStream::new(rx))
    }

    /// Returns a [`Doc`] client for a single document: `None` when the store
    /// holds no such document, an error when it holds one it cannot open.
    pub async fn open(&self, id: NamespaceId) -> Result<Option<Doc>> {
        match self.engine()?.sync.open(id, Default::default()).await {
            Ok(()) => Ok(Some(self.doc(id))),
            Err(err) if matches!(err.downcast_ref(), Some(OpenError::NotFound)) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// Document handle
#[derive(Debug, Clone)]
pub struct Doc {
    engine: Weak<Engine>,
    namespace_id: NamespaceId,
}

impl Doc {
    fn new(engine: Weak<Engine>, namespace_id: NamespaceId) -> Self {
        Self {
            engine,
            namespace_id,
        }
    }

    /// Returns the document id of this doc.
    pub fn id(&self) -> NamespaceId {
        self.namespace_id
    }

    fn engine(&self) -> Result<Arc<Engine>> {
        engine(&self.engine)
    }

    /// Sets the content of a key to a byte array, answering the hash of the
    /// payload it wrote.
    pub async fn set_bytes(
        &self,
        author_id: AuthorId,
        key: impl Into<Bytes>,
        value: impl Into<Bytes>,
    ) -> Result<Hash> {
        let engine = self.engine()?;
        let key = key.into();
        let value = value.into();
        let len = value.len();
        let tag = engine.blob_store().add_bytes(value).temp_tag().await?;
        let hash = tag.hash();
        let _held = HeldOnDrop {
            engine: &engine,
            tag: Some(tag),
        };
        engine
            .sync
            .insert_local(self.namespace_id, author_id, key, hash, len as u64)
            .await?;
        Ok(hash)
    }

    /// Deletes `author_id`'s entry at `key` by writing an empty entry there;
    /// no other key is touched.
    ///
    /// Returns 1 when an older entry at `key` was replaced, 0 otherwise.
    pub async fn del(&self, author_id: AuthorId, key: impl Into<Bytes>) -> Result<usize> {
        self.engine()?
            .sync
            .delete(self.namespace_id, author_id, key.into())
            .await
    }

    /// Returns all entries matching the query.
    pub async fn get_many(
        &self,
        query: impl Into<Query>,
    ) -> Result<impl Stream<Item = Result<Entry>>> {
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CAP);
        self.engine()?
            .sync
            .get_many(self.namespace_id, query.into(), tx)
            .await?;
        Ok(ReceiverStream::new(rx).map(|res| res.map(Entry::from)))
    }

    /// Returns a single entry.
    pub async fn get_one(&self, query: impl Into<Query>) -> Result<Option<Entry>> {
        let stream = self.get_many(query).await?;
        tokio::pin!(stream);
        n0_future::StreamExt::next(&mut stream).await.transpose()
    }

    /// Shares this document with peers over a ticket.
    pub async fn share(&self, addr_options: AddrInfoOptions) -> Result<DocTicket> {
        self.mint(addr_options, true).await
    }

    /// The ticket [`Self::share`] mints, leaving this document's sync as
    /// it is: a share's restart dials every peer the store recorded.
    pub async fn ticket(&self, addr_options: AddrInfoOptions) -> Result<DocTicket> {
        self.mint(addr_options, false).await
    }

    async fn mint(&self, addr_options: AddrInfoOptions, restart_sync: bool) -> Result<DocTicket> {
        let engine = self.engine()?;
        let me = addr_options.apply(&engine.endpoint.addr());
        let capability = Capability::Write(engine.sync.export_secret_key(self.namespace_id).await?);
        // Keeps whom unnamed peers are dialed as: an earlier start stated
        // it, and a share names no one.
        if restart_sync {
            engine.start_sync_keeping_default(self.namespace_id).await?;
        }
        Ok(DocTicket {
            capability,
            nodes: vec![me],
            identity: engine.identity(),
        })
    }

    /// Starts to sync this document with a list of peers, each paired
    /// with the identity it is dialed as.
    pub async fn start_sync(&self, peers: Vec<Contact>, default_identity: Identity) -> Result<()> {
        self.engine()?
            .start_sync(self.namespace_id, peers, default_identity)
            .await
    }

    /// Syncs this document with `peers` and the recorded peers in
    /// `recorded` alone, none of the others the engine recorded: for a
    /// caller that draws whom each run reaches. `join_gossip` joins the
    /// swarm as [`Self::start_sync`] does; without it the document stays
    /// outside, and updates arrive on the next sync alone.
    pub async fn sync_with_peers(
        &self,
        peers: Vec<Contact>,
        recorded: Vec<PeerIdBytes>,
        default_identity: Identity,
        join_gossip: bool,
    ) -> Result<()> {
        // Bytes that decode to no key name no node to dial, as a corrupt
        // entry in the recorded peers does.
        let recorded = recorded
            .iter()
            .filter_map(|peer| iroh::PublicKey::from_bytes(peer).ok())
            .collect();
        self.engine()?
            .sync_with_peers(
                self.namespace_id,
                peers,
                recorded,
                default_identity,
                join_gossip,
            )
            .await
    }

    /// Leaves this document's gossip swarm without stopping its live sync.
    ///
    /// The narrow inverse of the swarm join that [`Self::start_sync`]
    /// performs, for downgrading a replica to scoped access: the replica
    /// stays open and syncing — reconciliation keeps accepting and dialing,
    /// event subscribers stay live — while the gossip channel closes in
    /// both directions: nothing more is ingested from the topic, and local
    /// inserts stop broadcasting to it. Idempotent: leaving a swarm that
    /// was never joined does nothing; a later [`Self::start_sync`]
    /// re-joins.
    pub async fn leave_gossip(&self) -> Result<()> {
        self.engine()?.leave_gossip(self.namespace_id).await
    }

    /// Subscribes to events for this document.
    pub async fn subscribe(
        &self,
    ) -> Result<impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static> {
        self.engine()?.subscribe(self.namespace_id).await
    }

    /// Returns sync peers for this document
    pub async fn get_sync_peers(&self) -> Result<Option<Vec<PeerIdBytes>>> {
        self.engine()?.sync.get_sync_peers(self.namespace_id).await
    }
}
