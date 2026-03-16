use mee_sync_api::{AccessMode, NodeAddr, SubspaceId, SyncError, SyncHandle, SyncMode, SyncTicket};

// TODO(personal-namespaces): Add home_namespace() method that returns
// the node's personal namespace ID.
#[allow(async_fn_in_trait)]
pub trait SyncService: Send + Sync {
    async fn node_addr(&self) -> Result<NodeAddr, SyncError>;
    async fn subspace_id(&self) -> Result<SubspaceId, SyncError>;
    async fn share(&self, to: &SubspaceId, access: AccessMode) -> Result<SyncTicket, SyncError>;
    async fn import(
        &self,
        ticket: SyncTicket,
        mode: SyncMode,
    ) -> Result<Box<dyn SyncHandle>, SyncError>;
    async fn connect_to_peer(
        &self,
        to: &SubspaceId,
        peer_addr: &NodeAddr,
        access: AccessMode,
    ) -> Result<(), SyncError>;
}
