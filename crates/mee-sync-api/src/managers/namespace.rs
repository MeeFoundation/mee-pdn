use crate::{NamespaceId, SyncError};

#[allow(async_fn_in_trait)]
pub trait NamespaceManager: Send + Sync {
    async fn create_owned(&self) -> Result<NamespaceId, SyncError>;
    async fn list_owned(&self) -> Result<Vec<NamespaceId>, SyncError>;
}
