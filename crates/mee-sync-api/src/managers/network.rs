use crate::{NodeAddr, SyncError, TransportUserId};

#[allow(async_fn_in_trait)]
pub trait NetworkManager: Send + Sync {
    async fn addr(&self) -> Result<NodeAddr, SyncError>;
    async fn user_id(&self) -> Result<TransportUserId, SyncError>;
}
