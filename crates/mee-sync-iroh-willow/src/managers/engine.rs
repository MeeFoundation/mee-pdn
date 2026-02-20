use crate::{IrohWillowSyncCore, IrohWillowSyncHandle};
use mee_sync_api::managers as mgr;
use mee_sync_api::SyncError;

use super::{
    IrohWillowDataManager, IrohWillowDelegationManager, IrohWillowNamespaceManager,
    IrohWillowNetworkManager, IrohWillowSessionManager,
};

#[derive(Clone)]
pub struct IrohWillowSyncEngine {
    #[allow(dead_code)]
    engine: std::sync::Arc<IrohWillowSyncCore>,
    network: IrohWillowNetworkManager,
    namespaces: IrohWillowNamespaceManager,
    delegation: IrohWillowDelegationManager,
    sessions: IrohWillowSessionManager,
    data: IrohWillowDataManager,
}

impl IrohWillowSyncEngine {
    pub async fn spawn() -> Result<Self, SyncError> {
        let engine = std::sync::Arc::new(IrohWillowSyncCore::spawn().await?);
        Ok(Self {
            network: IrohWillowNetworkManager {
                engine: engine.clone(),
            },
            namespaces: IrohWillowNamespaceManager {
                engine: engine.clone(),
            },
            delegation: IrohWillowDelegationManager {
                engine: engine.clone(),
            },
            sessions: IrohWillowSessionManager {
                engine: engine.clone(),
            },
            data: IrohWillowDataManager {
                engine: engine.clone(),
            },
            engine,
        })
    }
}

impl mgr::SyncEngine for IrohWillowSyncEngine {
    type Network = IrohWillowNetworkManager;
    type Namespaces = IrohWillowNamespaceManager;
    type Delegation = IrohWillowDelegationManager;
    type Sessions = IrohWillowSessionManager;
    type Data = IrohWillowDataManager;

    type Handle = IrohWillowSyncHandle;
    type EntryStream = <IrohWillowDataManager as mgr::DataManager>::EntryStream;

    fn network(&self) -> &Self::Network {
        &self.network
    }
    fn namespaces(&self) -> &Self::Namespaces {
        &self.namespaces
    }
    fn delegation(&self) -> &Self::Delegation {
        &self.delegation
    }
    fn sessions(&self) -> &Self::Sessions {
        &self.sessions
    }
    fn data(&self) -> &Self::Data {
        &self.data
    }
}
