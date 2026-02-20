use crate::{EntryInfo, NamespaceId, SyncError};
use futures_core::Stream;

#[allow(async_fn_in_trait)]
pub trait DataManager: Send + Sync {
    type EntryStream: Stream<Item = Result<EntryInfo, SyncError>> + Send + Unpin + 'static;
    async fn insert(
        &self,
        ns: &NamespaceId,
        path: &crate::EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError>;
    async fn get_entries(&self, ns: &NamespaceId) -> Result<Self::EntryStream, SyncError>;
}
