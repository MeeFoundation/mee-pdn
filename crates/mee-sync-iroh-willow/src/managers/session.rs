use crate::{IrohWillowSyncCore, IrohWillowSyncHandle};
use mee_sync_api as api;
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncError;

#[derive(Clone)]
pub struct IrohWillowSessionManager {
    pub(crate) engine: std::sync::Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl mgr::SessionManager for IrohWillowSessionManager {
    type Handle = IrohWillowSyncHandle;

    async fn import_and_sync(
        &self,
        ticket: api::SyncTicket,
        _mode: api::SyncMode,
    ) -> Result<Self::Handle, SyncError> {
        let h = self.engine.import_and_sync_inner(ticket).await?;
        Ok(h)
    }
}
