use crate::IrohWillowSyncCore;
use mee_sync_api as api;
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncEngine as ApiSyncEngine;
use mee_sync_api::SyncError;

#[derive(Clone)]
pub struct IrohWillowDelegationManager {
    pub(crate) engine: std::sync::Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl mgr::DelegationManager for IrohWillowDelegationManager {
    async fn share(
        &self,
        ns: &api::NamespaceId,
        to: &api::TransportUserId,
        access: api::AccessMode,
    ) -> Result<api::SyncTicket, SyncError> {
        self.engine.share(ns, to, access).await
    }
}
