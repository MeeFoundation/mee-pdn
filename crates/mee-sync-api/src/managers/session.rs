use crate::{SyncError, SyncHandle, SyncMode, SyncTicket};

#[allow(async_fn_in_trait)]
pub trait SessionManager: Send + Sync {
    type Handle: SyncHandle;
    async fn import_and_sync(
        &self,
        ticket: SyncTicket,
        mode: SyncMode,
    ) -> Result<Self::Handle, SyncError>;
}
