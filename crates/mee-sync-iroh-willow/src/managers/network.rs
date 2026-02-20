use crate::IrohWillowSyncCore;
use mee_sync_api as api;
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncEngine as ApiSyncEngine;
use mee_sync_api::SyncError;

#[derive(Clone)]
pub struct IrohWillowNetworkManager {
    pub(crate) engine: std::sync::Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl mgr::NetworkManager for IrohWillowNetworkManager {
    async fn addr(&self) -> Result<api::NodeAddr, SyncError> {
        self.engine.addr().await
    }
    async fn user_id(&self) -> Result<api::TransportUserId, SyncError> {
        self.engine.user_id().await
    }
}
