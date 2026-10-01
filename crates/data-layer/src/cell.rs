//! A cell's two stores, the membership store and the record store, by the
//! cell stores spec.

mod fold;
mod keys;
mod past;
mod payloads;
mod record_view;
#[cfg(test)]
mod testing;

use anyhow::Result;
use futures_lite::StreamExt;
use pdn_store::{api::Doc, store::Query, AuthorId, DocTicket};
use pdn_types::{CellId, PdnId};

pub use fold::{Awaiting, ForNothing, HeldEntry, Member, MemberState, Membership, Verdict};
pub use keys::{record_prefix, EventKind, MembershipKey, OpId, RecordKey, Seq};
pub(crate) use past::{departure_past, PastEntry};
pub(crate) use payloads::encode_devices;
pub use payloads::{DevicesPayload, FoundedPayload, JoinedPayload, MemberDevice, ACT_PAYLOAD};
pub use record_view::{Operation, RecordEntry, RecordView};

/// `identity` holds no cell `cell` here, or holds only its tombstone.
/// Downcast from the `anyhow::Error` of the cell-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("cell not held on this node: {cell}")]
pub struct UnknownCell {
    pub cell: CellId,
}

/// Which of a cell's two stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellStore {
    Membership,
    Records,
}

/// The fold's verdicts on one run over one identity's replica of a cell's
/// membership store: each entry by its key and author.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone)]
pub struct CellVerdicts {
    pub identity: pdn_types::PdnId,
    pub cell: CellId,
    pub verdicts: Vec<(Vec<u8>, pdn_store::AuthorId, Verdict)>,
}

/// Every entry of `doc`, one per author and key as the store keeps them,
/// each with its payload once the bytes have arrived.
pub(crate) async fn held_entries(
    doc: &Doc,
    blobs: &iroh_blobs::api::Store,
) -> Result<Vec<HeldEntry>> {
    let mut held = Vec::new();
    let mut stream = std::pin::pin!(doc.get_many(Query::all()).await?);
    while let Some(entry) = stream.next().await {
        let entry = entry?;
        let hash = entry.content_hash();
        let payload = if blobs.has(hash).await? {
            Some(blobs.get_bytes(hash).await?.to_vec())
        } else {
            None
        };
        held.push(HeldEntry {
            key: entry.key().to_vec(),
            author: entry.author(),
            payload,
        });
    }
    Ok(held)
}

/// The entries of a record store `query` selects, one per author and key
/// as the store keeps them, each with its content hash once the bytes have
/// arrived.
pub(crate) async fn record_entries(
    doc: &Doc,
    blobs: &iroh_blobs::api::Store,
    query: impl Into<Query>,
) -> Result<Vec<RecordEntry>> {
    let mut held = Vec::new();
    let mut stream = std::pin::pin!(doc.get_many(query).await?);
    while let Some(entry) = stream.next().await {
        let entry = entry?;
        let hash = entry.content_hash();
        held.push(RecordEntry {
            key: entry.key().to_vec(),
            author: entry.author(),
            timestamp: entry.timestamp(),
            payload: blobs.has(hash).await?.then_some(hash),
        });
    }
    Ok(held)
}

/// An entry of either store whose key fits no layout of its store: kept,
/// read by nothing, and listed with its author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownEntry {
    pub store: CellStore,
    pub key: Vec<u8>,
    pub author: AuthorId,
}

/// The entries of `doc`, one of `cell`'s stores, whose keys fit no layout of
/// that store.
pub(crate) async fn unknown_entries(doc: &Doc, store: CellStore) -> Result<Vec<UnknownEntry>> {
    let fits = |key: &[u8]| match store {
        CellStore::Membership => MembershipKey::parse(key).is_some(),
        CellStore::Records => RecordKey::parse(key).is_some(),
    };
    let mut unknown = Vec::new();
    let mut stream = std::pin::pin!(doc.get_many(Query::all()).await?);
    while let Some(entry) = stream.next().await {
        let entry = entry?;
        if !fits(entry.key()) {
            unknown.push(UnknownEntry {
                store,
                key: entry.key().to_vec(),
                author: entry.author(),
            });
        }
    }
    Ok(unknown)
}

/// `identity`'s chain in `cell` ends in a counted left or kicked event at
/// `seq` while its device still holds the cell's record store, from
/// [`SyncNode::take_cell_departures`](crate::SyncNode::take_cell_departures).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDeparture {
    pub identity: PdnId,
    pub cell: CellId,
    pub seq: Seq,
}

/// Empty until the channel is taken, so nothing accumulates unread.
pub(crate) type CellDepartureSink =
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<CellDeparture>>>>;

/// A wait for each of a cell's stores' first successful session started
/// since an import, from [`SyncNode::import_cell`](crate::SyncNode::import_cell).
#[derive(Debug)]
pub struct CellCatchUp {
    pub(crate) membership: crate::private_metadata::CatchUpWatch,
    pub(crate) records: crate::private_metadata::CatchUpWatch,
}

impl CellCatchUp {
    /// Fails with [`CatchUpTimeout`](crate::CatchUpTimeout) when either store
    /// has had none within `timeout`.
    pub async fn wait(self, timeout: std::time::Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        self.membership.wait(timeout).await?;
        self.records
            .wait(deadline.saturating_duration_since(std::time::Instant::now()))
            .await
    }
}

/// The write tickets to a cell's two stores.
#[derive(Debug, Clone)]
pub struct CellTickets {
    pub membership: DocTicket,
    pub records: DocTicket,
}
