//! A cell's two stores, the membership store and the record store, by the
//! cell stores spec.

mod fold;
mod keys;
mod payloads;

use pdn_store::DocTicket;
use pdn_types::CellId;

pub use fold::{Awaiting, ForNothing, HeldEntry, Member, MemberState, Membership, Verdict};
pub use keys::{record_prefix, EventKind, MembershipKey, OpId, RecordKey, Seq};
pub(crate) use payloads::encode_devices;
pub use payloads::{DevicesPayload, FoundedPayload, JoinedPayload, MemberDevice};

/// `identity` holds no cell `cell` here, or holds only its tombstone.
/// Downcast from the `anyhow::Error` of the cell-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("cell not held on this node: {cell}")]
pub struct UnknownCell {
    pub cell: CellId,
}

/// The write tickets to a cell's two stores.
#[derive(Debug, Clone)]
pub struct CellTickets {
    pub membership: DocTicket,
    pub records: DocTicket,
}
