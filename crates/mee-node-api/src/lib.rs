use mee_did_api::DidManager;
use mee_local_store_api::KvStore;
use mee_transport_api::{ProfileName, Transport};
use mee_types::{NodeId, UserId};

pub trait Node {
    type Transport: Transport;
    type DidManager: DidManager;
    type Store: KvStore;

    fn profile(&self) -> &ProfileName;
    fn node_id(&self) -> &NodeId;
    fn user_id(&self) -> Option<&UserId>;
    fn transport(&self) -> &Self::Transport;
    fn did_manager(&self) -> &Self::DidManager;
    fn store(&self) -> &Self::Store;
}
