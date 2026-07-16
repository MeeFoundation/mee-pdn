//! The private metadata store: a device-replicated registry of an identity's
//! own infrastructure — its devices and the tickets to its other stores.
//!
//! A dedicated pdn-store replica, separate from data namespaces, that all
//! devices of one identity replicate. It is device-internal by ticket alone
//! (Invariant 1's remaining mechanism —
//! `mia-docs/openspec/specs/components/pdn-node/invariants.md`): its ticket is
//! the seed handed only at device linking, and no ingest filter runs. It is
//! the bootstrap **directory**: a newly linked device reads the device list
//! and the typed tickets here to find and import the identity's other stores.
//! Linking is gradual — a minimal access seed first, then the rest of the
//! tickets and data sync in over this store. One node holds the private
//! metadata stores of any number of identities.
//!
//! Device records are record-level (visible as soon as the entry syncs);
//! ticket payloads are blobs, so `get_ticket` returns `None` until the
//! payload has arrived.

use anyhow::Result;
use futures_core::Stream;
use futures_lite::StreamExt;
use iroh::EndpointAddr;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    engine::LiveEvent,
    store::Query,
    AuthorId, DocTicket,
};
use pdn_types::NodeId;

use crate::node::{read_payload, SyncNode};

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
/// device reads from. The owning identity is not kept here — the handle's
/// holder knows which identity it serves.
#[derive(Debug)]
pub struct PrivateMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
}

impl PrivateMetadataStore {
    /// Create a fresh private metadata store on `node`.
    pub async fn create(node: &SyncNode) -> Result<Self> {
        let doc = node.new_doc().await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Import an existing private metadata store via `ticket` (the access seed
    /// handed to a newly linked device).
    pub async fn import(node: &SyncNode, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_doc(ticket).await?;
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

    /// (Re-)request a sync of this store's replica with `peers` — the
    /// registration push of device linking. The engine also includes every
    /// peer the replica has synced with before; a request made while a
    /// session is already running is dropped, so callers retry off the
    /// session-finished events ([`events`](Self::events)).
    pub(crate) async fn sync_with(&self, peers: Vec<EndpointAddr>) -> Result<()> {
        self.doc.start_sync(peers).await?;
        Ok(())
    }

    /// Subscribe to this store's replica events (inserts, sync sessions).
    pub(crate) async fn events(
        &self,
    ) -> Result<impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static> {
        self.doc.subscribe().await
    }

    /// List the kinds under which tickets are currently published
    /// (record-level — available as soon as the records sync; a listed
    /// kind's ticket may still be payload-waiting in
    /// [`get_ticket`](Self::get_ticket)). The directory's audit surface:
    /// what routing the identity's devices can discover here — and, per the
    /// routing/grants boundary, what must not appear (no tickets to another
    /// identity's data stores; those live in connection metadata stores).
    pub async fn list_ticket_kinds(&self) -> Result<Vec<String>> {
        let query = Query::single_latest_per_key().key_prefix(TICKETS_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut kinds = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            let Ok(key) = std::str::from_utf8(entry.key()) else {
                continue;
            };
            if let Some(kind) = key.strip_prefix(TICKETS_PREFIX) {
                kinds.push(kind.to_owned());
            }
        }
        Ok(kinds)
    }

    /// Read the stored ticket for store `kind`, if present and its payload has
    /// arrived. Returns `Ok(None)` while the payload is still syncing.
    ///
    /// A payload that does not decode is an error, not an absence: the only
    /// writers here are the identity's own devices, so garbage is this
    /// implementation's own bug. The counterparty-written grants of
    /// [`ConnectionMetadataStore::read_grant`](crate::ConnectionMetadataStore::read_grant)
    /// deliberately read the other way.
    pub async fn get_ticket(&self, kind: &str) -> Result<Option<DocTicket>> {
        let Some(bytes) = read_payload(&self.doc, &self.blobs, ticket_key(kind).as_bytes()).await?
        else {
            return Ok(None);
        };
        let ticket = std::str::from_utf8(&bytes)?.parse::<DocTicket>()?;
        Ok(Some(ticket))
    }
}
