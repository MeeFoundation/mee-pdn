//! The [`DataLayer`] trait: the entries-only contract the node runtime
//! drives to read and write replicated entries.

use futures_core::Stream;
use pdn_types::{EntryInfo, EntryPath, PdnId};

/// Error returned by [`DataLayer`] operations.
#[derive(Debug, thiserror::Error)]
pub enum DataLayerError {
    /// The local node does not control a device key resolvable to `issued_by`.
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
/// live in that issuer's single replica. Writing does NOT require any
/// subject's consent — only local authority over `issuer` is checked; the
/// subject (`about`) lives inside the entry payload, not in the address.
///
/// Capability semantics (issuing, revocation, chain validation) live above
/// this trait, in the PDN layer: tokens travel as ordinary entries.
/// Enforcement below this trait arrives with subset-rbsr (egress filtering)
/// and `UWill`; until then access to a replica is bounded by possession of
/// its ticket.
///
/// This authorization model requires pdn-store, our fork; plain
/// namespace-key authorization cannot express PdnId-based write authority.
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
    ///
    /// Yields metadata only (no payload bytes); use
    /// [`get_entry`](Self::get_entry) to fetch payloads for entries of
    /// interest.
    async fn list_entries(
        &self,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Self::EntryStream, DataLayerError>;
}
