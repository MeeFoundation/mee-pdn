use crate::IrohWillowSyncCore;
use mee_sync_api as api;
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncEngine;
use mee_sync_api::SyncError;

#[derive(Clone)]
pub struct IrohWillowDataManager {
    pub(crate) engine: std::sync::Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl mgr::DataManager for IrohWillowDataManager {
    type EntryStream = <IrohWillowSyncCore as SyncEngine>::EntryStream;

    async fn insert(
        &self,
        ns: &api::NamespaceId,
        path: &api::EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError> {
        self.engine.insert(ns, path, bytes).await
    }

    async fn get_entries(&self, ns: &api::NamespaceId) -> Result<Self::EntryStream, SyncError> {
        self.engine.get_entries(ns).await
    }
}
