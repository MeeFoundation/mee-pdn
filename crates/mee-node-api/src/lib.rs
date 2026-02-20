pub use data_service::{DataEntry, DataService};
pub use identity_service::IdentityService;
use mee_local_store_api::KvStore;
use mee_types::NodeId;
pub use sync_service::SyncService;
pub use trust_service::{Contact, Invite, InviteSignature, TrustService};

mod data_service;
mod identity_service;
mod sync_service;
mod trust_service;

pub trait Node {
    type Store: KvStore;
    type Identity: IdentityService;
    type Trust: TrustService;
    type Data: DataService;
    type Sync: SyncService;

    fn node_id(&self) -> &NodeId;
    fn store(&self) -> &Self::Store;
    fn identity(&self) -> &Self::Identity;
    fn trust(&self) -> &Self::Trust;
    fn data(&self) -> &Self::Data;
    fn sync(&self) -> &Self::Sync;
}
