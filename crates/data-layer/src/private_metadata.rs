//! The private metadata store: a device-replicated registry of an identity's
//! own infrastructure — its devices and the tickets to its other stores.
//!
//! A dedicated pdn-store replica, separate from data namespaces, that all
//! devices of one identity replicate (device-internal: the gate enforces
//! Invariant 1 — `mia-docs/openspec/specs/components/pdn-node/invariants.md`). It is
//! the bootstrap **directory**: a newly linked device reads the device list
//! and the typed tickets here to find and import the identity's other stores.
//! Linking is gradual — a minimal access seed first, then the rest of the
//! tickets and data sync in over this store.
//!
//! Device records are record-level (visible as soon as the entry syncs);
//! ticket payloads are blobs, so `get_ticket` returns `None` until the
//! payload has arrived.

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
use pdn_types::{NodeId, PdnId};

use crate::node::SyncNode;

/// Key prefix for device records.
const DEVICES_PREFIX: &str = "devices/";
/// Key prefix for typed tickets.
const TICKETS_PREFIX: &str = "tickets/";

fn device_key(device: &NodeId) -> String {
    format!("{DEVICES_PREFIX}{device}")
}

fn ticket_key(kind: &str) -> String {
    format!("{TICKETS_PREFIX}{kind}")
}

fn device_of(key: &[u8]) -> Option<NodeId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(DEVICES_PREFIX)?
        .parse()
        .ok()
}

/// Device-replicated registry of an identity's own metadata: its devices and
/// the tickets to its other stores. The bootstrap directory a newly linked
/// device reads from. The owning identity is not kept here — it lives in the
/// registry binding the gate reads.
#[derive(Debug)]
pub struct PrivateMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
}

impl PrivateMetadataStore {
    /// Create a fresh private metadata store on `node`, bound as
    /// `PrivateMetadata { identity }`.
    pub async fn create(node: &mut SyncNode, identity: PdnId) -> Result<Self> {
        let doc = node.new_private_metadata_doc(identity).await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Import an existing private metadata store via `ticket` (the access seed
    /// handed to a newly linked device), bound as `PrivateMetadata { identity }`.
    pub async fn import(node: &mut SyncNode, identity: PdnId, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_private_metadata_doc(identity, ticket).await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Share this store as a ticket — the access seed for linking a device.
    pub async fn share_ticket(
        &self,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// Record `device` as one of the identity's devices.
    pub async fn add_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .set_bytes(self.author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// List the identity's known devices (record-level — available as soon as
    /// the records sync).
    pub async fn list_devices(&self) -> Result<Vec<NodeId>> {
        let query = Query::single_latest_per_key().key_prefix(DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut devices = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(device) = device_of(entry?.key()) {
                devices.push(device);
            }
        }
        Ok(devices)
    }

    /// Store the `ticket` for store `kind` (e.g. `"connections"`, `"data"`),
    /// so a linked device can discover and import that store.
    pub async fn put_ticket(&self, kind: &str, ticket: &DocTicket) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                ticket_key(kind).into_bytes(),
                ticket.to_string().into_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Read the stored ticket for store `kind`, if present and its payload has
    /// arrived. Returns `Ok(None)` while the payload is still syncing.
    pub async fn get_ticket(&self, kind: &str) -> Result<Option<DocTicket>> {
        let query = Query::single_latest_per_key().key_exact(ticket_key(kind).as_bytes());
        let Some(entry) = self.doc.get_one(query).await? else {
            return Ok(None);
        };
        let hash = entry.content_hash();
        if !self.blobs.has(hash).await? {
            return Ok(None);
        }
        let bytes = self.blobs.get_bytes(hash).await?.to_vec();
        let ticket = std::str::from_utf8(&bytes)?.parse::<DocTicket>()?;
        Ok(Some(ticket))
    }
}
