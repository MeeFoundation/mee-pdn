//! The [`DataLayer`] trait: the entries-only contract the node runtime
//! drives to read and write replicated entries.

use futures_core::Stream;
use pdn_types::{EntryInfo, EntryPath, NamespaceId, PdnId};

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
/// A [`NamespaceId`] is structurally the pair `(about, issued_by)`:
/// `issued_by` is the sole writer/owner; `about` is the subject the entry
/// concerns. Writing into `namespace` does NOT require `namespace.about`'s
/// consent — only local authority over `namespace.issued_by` is checked.
///
/// Capability semantics (issuing, revocation, chain validation) live above
/// this trait, in the PDN layer: tokens travel as ordinary entries, and
/// ingest-time checks are injected into the backend as an
/// [`IngestPolicy`](crate::IngestPolicy).
///
/// This authorization model requires pdn-store, our capability-gated
/// fork; plain namespace-key authorization cannot express PdnId-based
/// write authority.
#[allow(async_fn_in_trait)]
pub trait DataLayer: Send + Sync {
    /// Insert `payload` at `path` into `namespace`.
    async fn insert_entry(
        &self,
        namespace: &NamespaceId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<(), DataLayerError>;

    /// Read the payload bytes for the entry at `path` in `namespace`.
    /// Returns `Ok(None)` if no such entry exists.
    async fn get_entry(
        &self,
        namespace: &NamespaceId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>, DataLayerError>;

    /// Stream type yielding entry metadata for [`list_entries`](Self::list_entries).
    type EntryStream: Stream<Item = Result<EntryInfo, DataLayerError>> + Send + Unpin + 'static;

    /// Enumerate metadata for entries in `namespace`, optionally filtered
    /// to those whose `path` starts with `path_prefix`.
    ///
    /// Yields metadata only (no payload bytes); use
    /// [`get_entry`](Self::get_entry) to fetch payloads for entries of
    /// interest.
    async fn list_entries(
        &self,
        namespace: &NamespaceId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Self::EntryStream, DataLayerError>;
}
