use mee_sync_api::{
    EntryInfo, EntryPath, NodeAddr, SyncError, SyncHandle, SyncMode, SyncTicket, TransportUserId,
};

#[allow(async_fn_in_trait)]
pub trait SyncService: Send + Sync {
    async fn node_addr(&self) -> Result<NodeAddr, SyncError>;
    async fn user_id(&self) -> Result<TransportUserId, SyncError>;
    async fn share(&self, to: &TransportUserId, write: bool) -> Result<SyncTicket, SyncError>;
    async fn import(
        &self,
        ticket: SyncTicket,
        mode: SyncMode,
    ) -> Result<Box<dyn SyncHandle>, SyncError>;
    async fn connect_to_peer(
        &self,
        to: &TransportUserId,
        peer_addr: &NodeAddr,
        write: bool,
    ) -> Result<(), SyncError>;
    async fn insert(&self, path: &EntryPath, bytes: &[u8]) -> Result<(), SyncError>;
    async fn list(&self) -> Result<Vec<EntryInfo>, SyncError>;
}
