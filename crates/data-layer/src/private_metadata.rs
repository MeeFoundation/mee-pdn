//! The private metadata store: the one device-replicated **directory** of an
//! identity's own state — its devices, the tickets to its other stores, and
//! its connections.
//!
//! A dedicated pdn-store replica, separate from data namespaces, that all
//! devices of one identity replicate. It is device-internal by ticket alone
//! (Invariant 1's remaining mechanism —
//! `mia-docs/openspec/specs/components/pdn-node/invariants.md`): its ticket
//! is handed only to the identity's own devices (over the device-linking
//! dialogue), and no ingest filter runs. Three record families live here,
//! under disjoint prefixes: `devices/` — the device set; `tickets/` — typed
//! tickets to the identity's other stores and its connections' metadata
//! pairs; `connections/` — one marker record per connection counterparty.
//! One node holds the private metadata stores of any number of identities.
//!
//! Device and connection records are record-level (visible as soon as the
//! entry syncs — liveness never waits on payload bytes); ticket payloads are
//! blobs, so `get_ticket` returns `None` until the payload has arrived.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use futures_core::Stream;
use futures_lite::StreamExt;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    engine::LiveEvent,
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use pdn_types::{NodeId, PdnId};

use crate::node::{read_payload, SyncNode};

/// The bounded wait of [`PrivateMetadataStore::wait_caught_up`] elapsed with
/// no successful sync session of the replica started after the given
/// instant. Downcast from the `anyhow::Error` of that wait — how a caller
/// tells "did not catch up in time" apart from this node's own failures.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no successful sync session of the replica within the wait")]
pub struct CatchUpTimeout;

/// Key prefix for device records.
const DEVICES_PREFIX: &str = "devices/";
/// Key prefix for typed tickets.
const TICKETS_PREFIX: &str = "tickets/";
/// Key prefix for connection records.
const CONNECTIONS_PREFIX: &str = "connections/";

fn device_key(device: &NodeId) -> String {
    format!("{DEVICES_PREFIX}{device}")
}

fn ticket_key(kind: &str) -> String {
    format!("{TICKETS_PREFIX}{kind}")
}

/// The entry key for a connection to `peer`: `connections/<pdnid-hex>`.
fn connection_key(peer: &PdnId) -> String {
    format!("{CONNECTIONS_PREFIX}{peer}")
}

fn device_of(key: &[u8]) -> Option<NodeId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(DEVICES_PREFIX)?
        .parse()
        .ok()
}

/// Parse a `PdnId` back out of a `connections/<hex>` key, if it matches.
fn connection_peer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(CONNECTIONS_PREFIX)?
        .parse()
        .ok()
}

/// Device-replicated directory of an identity's own state: its devices, the
/// tickets to its other stores, and its connections. The owning identity is
/// not kept here — the handle's holder knows which identity it serves.
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

    /// Import an existing private metadata store via `ticket` (the write
    /// ticket handed to a newly linked device over the linking dialogue).
    pub async fn import(node: &SyncNode, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_doc(ticket).await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Share this store as a ticket another device of the identity can
    /// import.
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

    /// Record a live connection to `peer`. The payload is an opaque marker
    /// (the key carries the identity), replicated to the identity's other
    /// devices like any directory entry.
    pub async fn connect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .set_bytes(self.author, connection_key(&peer).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Drop the connection to `peer` — writes a tombstone (empty entry) that
    /// replicates like any other entry.
    pub async fn disconnect(&self, peer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, connection_key(&peer).into_bytes())
            .await?;
        Ok(())
    }

    /// Whether `peer` is currently a live connection.
    ///
    /// A record-level check: it returns `true` as soon as the connect entry
    /// is present, without waiting on the marker blob to download. A
    /// tombstone (latest entry empty) reads as not connected.
    pub async fn is_connected(&self, peer: PdnId) -> Result<bool> {
        let query = Query::single_latest_per_key().key_exact(connection_key(&peer).as_bytes());
        Ok(self.doc.get_one(query).await?.is_some())
    }

    /// List currently live connections (record-level, like
    /// [`is_connected`](Self::is_connected)).
    pub async fn list_connections(&self) -> Result<Vec<PdnId>> {
        let query = Query::single_latest_per_key().key_prefix(CONNECTIONS_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut peers = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(peer) = connection_peer_of(entry?.key()) {
                peers.push(peer);
            }
        }
        Ok(peers)
    }

    /// Store the `ticket` for store `kind` (e.g. `"data"`), so the
    /// identity's other devices can discover and import that store.
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

    /// Subscribe to this store's replica events (inserts, sync sessions).
    /// Crate-private on purpose: the fork's event type stays behind this
    /// layer, and the one property consumers need is stated by
    /// [`wait_caught_up`](Self::wait_caught_up).
    pub(crate) async fn events(
        &self,
    ) -> Result<impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static> {
        self.doc.subscribe().await
    }

    /// The namespace id of the backing replica — which replica this handle
    /// addresses. Lets an imported directory be named to
    /// [`SyncNode::forget_doc`] when the act that imported it fails and must
    /// leave nothing behind.
    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
    }

    /// Wait until the first successful sync session of this replica that
    /// started after `since` has finished, or fail with [`CatchUpTimeout`]
    /// once `timeout` elapses — never hang.
    ///
    /// The property waited on is "this replica has caught up with a peer":
    /// a completed, successful exchange — not "some content arrived".
    /// Polling contents cannot state it: a replica that synced and found
    /// nothing new and one that never synced read the same. No trigger is
    /// offered or needed — importing a replica already starts its first
    /// session and enrols it in the node's periodic reconcile pass with the
    /// ticket's contacts, so a first exchange that fails is re-dialed
    /// within this wait's own budget.
    pub async fn wait_caught_up(&self, since: SystemTime, timeout: Duration) -> Result<()> {
        let mut events = self.events().await?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CatchUpTimeout.into());
            }
            let Ok(event) = tokio::time::timeout(remaining, events.next()).await else {
                return Err(CatchUpTimeout.into());
            };
            let event =
                event.context("replica event stream ended while waiting for a sync session")??;
            if let LiveEvent::SyncFinished(sync) = event {
                if sync.result.is_ok() && sync.started >= since {
                    return Ok(());
                }
            }
        }
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
