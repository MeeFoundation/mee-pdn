//! The connections store: a device-replicated registry of an identity's
//! connections.
//!
//! A dedicated pdn-store replica, separate from data namespaces, that all
//! devices of one identity replicate. One entry per connection at
//! `connections/<pdnid-hex>`; the payload is an opaque marker (the key
//! carries the identity), and `disconnect` writes a tombstone (empty entry).
//! Liveness is a record-level fact — `is_connected` never waits on blob
//! content.
//!
//! Its entries are admitted because the gate enforces Invariant 1 (the
//! `SelfOwned` policy — a node admits its own identity's replicas); having the
//! gate *read* this store to admit data is a deferred follow-up.

use anyhow::Result;
use futures_lite::StreamExt;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    store::Query,
    AuthorId, DocTicket,
};
use pdn_types::PdnId;

use crate::node::SyncNode;

/// Key prefix under which connection entries live.
const PREFIX: &str = "connections/";

/// The entry key for a connection to `peer`: `connections/<pdnid-hex>`.
fn key_for(peer: &PdnId) -> String {
    format!("{PREFIX}{peer}")
}

/// Parse a `PdnId` back out of a `connections/<hex>` key, if it matches.
fn peer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(PREFIX)?
        .parse()
        .ok()
}

/// Device-replicated registry of an identity's connections.
///
/// Built from a [`SyncNode`]; holds the backing replica and an author for
/// local writes. The owning identity is not kept here — it lives in the
/// registry binding the gate reads. Reads (`is_connected`, `list`) go through
/// ordinary doc queries and are never used by the gate.
#[derive(Debug)]
pub struct ConnectionsStore {
    doc: Doc,
    author: AuthorId,
}

impl ConnectionsStore {
    /// Create a fresh connections store on `node`, bound as
    /// `Connections { identity }`.
    pub async fn create(node: &mut SyncNode, identity: PdnId) -> Result<Self> {
        let doc = node.new_connections_doc(identity).await?;
        let author = node.create_author().await?;
        Ok(Self { doc, author })
    }

    /// Import an existing connections store via `ticket` (device linking),
    /// bound as `Connections { identity }`.
    pub async fn import(node: &mut SyncNode, identity: PdnId, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_connections_doc(identity, ticket).await?;
        let author = node.create_author().await?;
        Ok(Self { doc, author })
    }

    /// Share this store as a ticket another device can import.
    pub async fn share_ticket(
        &self,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// Record a live connection to `peer`.
    pub async fn connect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .set_bytes(self.author, key_for(&peer).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Drop the connection to `peer` — writes a tombstone (empty entry) that
    /// replicates like any other entry.
    pub async fn disconnect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, key_for(&peer).into_bytes())
            .await?;
        Ok(())
    }

    /// Whether `peer` is currently a live connection.
    ///
    /// A record-level check: it returns `true` as soon as the connect entry
    /// is present, without waiting on the marker blob to download. A
    /// tombstone (latest entry empty) reads as not connected.
    pub async fn is_connected(&self, peer: PdnId) -> Result<bool> {
        let query = Query::single_latest_per_key().key_exact(key_for(&peer).as_bytes());
        Ok(self.doc.get_one(query).await?.is_some())
    }

    /// List currently live connections.
    pub async fn list(&self) -> Result<Vec<PdnId>> {
        let query = Query::single_latest_per_key().key_prefix(PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut peers = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(peer) = peer_of(entry?.key()) {
                peers.push(peer);
            }
        }
        Ok(peers)
    }
}
