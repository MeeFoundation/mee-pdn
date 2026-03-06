pub use data_service::{DataEntry, DataError, DataService};
pub use identity_service::IdentityService;
use mee_types::NodeId;
pub use sync_service::SyncService;
pub use trust_service::{Contact, Invite, InviteSignature, TrustService};

mod data_service;
mod identity_service;
mod sync_service;
mod trust_service;

pub trait Node {
    type Identity: IdentityService;
    type Trust: TrustService;
    type Data: DataService;
    type Sync: SyncService;

    fn node_id(&self) -> &NodeId;
    fn identity(&self) -> &Self::Identity;
    fn trust(&self) -> &Self::Trust;
    fn data(&self) -> &Self::Data;
    fn sync(&self) -> &Self::Sync;
}
