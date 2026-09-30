//! A cell's two stores, the membership store and the record store, by the
//! cell stores spec.

mod fold;
mod keys;
mod payloads;
mod record_view;
#[cfg(test)]
mod testing;

use anyhow::Result;
use futures_lite::StreamExt;
use pdn_store::{api::Doc, store::Query, DocTicket};
use pdn_types::CellId;

pub use fold::{Awaiting, ForNothing, HeldEntry, Member, MemberState, Membership, Verdict};
pub use keys::{record_prefix, EventKind, MembershipKey, OpId, RecordKey, Seq};
pub(crate) use payloads::encode_devices;
pub use payloads::{DevicesPayload, FoundedPayload, JoinedPayload, MemberDevice};
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

/// The write tickets to a cell's two stores.
#[derive(Debug, Clone)]
pub struct CellTickets {
    pub membership: DocTicket,
    pub records: DocTicket,
}
