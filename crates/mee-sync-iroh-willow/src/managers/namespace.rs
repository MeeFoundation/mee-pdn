use crate::IrohWillowSyncCore;
use mee_sync_api as api;
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncEngine as ApiSyncEngine;
use mee_sync_api::SyncError;

#[derive(Clone)]
pub struct IrohWillowNamespaceManager {
    pub(crate) engine: std::sync::Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl mgr::NamespaceManager for IrohWillowNamespaceManager {
    async fn create_owned(&self) -> Result<api::NamespaceId, SyncError> {
        let owner = self.engine.user_id().await?;
        self.engine.create_namespace(&owner).await
    }
    async fn list_owned(&self) -> Result<Vec<api::NamespaceId>, SyncError> {
        // Placeholder: Currently returns all tracked namespaces (owned + imported)
        self.engine.list_namespaces().await
    }
}
