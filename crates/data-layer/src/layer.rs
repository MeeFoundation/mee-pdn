//! The [`DataLayer`] trait: an entries-only contract over the data layer.
//! Declared only — nothing implements it; the runtime drives
//! [`SyncNode`](crate::SyncNode) directly.

use futures_core::Stream;
use pdn_types::{EntryInfo, EntryPath, PdnId};

/// Error returned by [`DataLayer`] operations.
#[derive(Debug, thiserror::Error)]
pub enum DataLayerError {
    /// The local node holds no authority to write as `issued_by`.
    #[error("local node is not authorized to write as {issued_by}")]
    NotAuthorizedToWrite { issued_by: PdnId },

    /// Payload exceeds the backend's maximum payload size.
    #[error("payload too large: {size} bytes (max {max})")]
    PayloadTooLarge { size: usize, max: usize },

    /// Underlying storage/sync backend reported an error.
    #[error("storage backend error")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Entries-only interface of the data layer.
///
/// Data is keyed by its `issuer` (a [`PdnId`]): all of an issuer's entries
/// live in that issuer's single replica. The subject (`about`) lives inside
/// the entry payload, not in the address.
#[allow(async_fn_in_trait)]
pub trait DataLayer: Send + Sync {
    /// Insert `payload` at `path` into the data namespace of `issuer`.
    async fn insert_entry(
        &self,
        issuer: PdnId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<(), DataLayerError>;

    /// Read the payload bytes for the entry at `path` in the data namespace of
    /// `issuer`. Returns `Ok(None)` if no such entry exists.
    async fn get_entry(
        &self,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>, DataLayerError>;

    /// Stream type yielding entry metadata for [`list_entries`](Self::list_entries).
    type EntryStream: Stream<Item = Result<EntryInfo, DataLayerError>> + Send + Unpin + 'static;

    /// Enumerate metadata for entries in the data namespace of `issuer`,
    /// optionally filtered to those whose `path` starts with `path_prefix`.
    /// No payload bytes; [`get_entry`](Self::get_entry) fetches them.
    async fn list_entries(
        &self,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Self::EntryStream, DataLayerError>;
}
