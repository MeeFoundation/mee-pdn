use crate::{AccessMode, NamespaceId, SyncError, SyncTicket, TransportUserId};

#[allow(async_fn_in_trait)]
pub trait DelegationManager: Send + Sync {
    async fn share(
        &self,
        ns: &NamespaceId,
        to: &TransportUserId,
        access: AccessMode,
    ) -> Result<SyncTicket, SyncError>;
}
