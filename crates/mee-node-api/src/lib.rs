pub use data_service::{DataEntry, DataError, DataService};
pub use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityResolver, IdentityState, KeyAtResult, KeyStatus,
};
use mee_sync_api::NamespaceId;
use mee_types::NodeId;
pub use sync_service::SyncService;
pub use trust_service::{Contact, Invite, InviteSignature, TrustService};

mod data_service;
mod sync_service;
mod trust_service;

pub trait Node {
    type IdentityProvider: IdentityProvider;
    type IdentityResolver: IdentityResolver;
    type Trust: TrustService;
    type Data: DataService;
    type Sync: SyncService;

    fn node_id(&self) -> &NodeId;
    fn home_namespace(&self) -> NamespaceId;
    fn identity_provider(&self) -> &Self::IdentityProvider;
    fn identity_resolver(&self) -> &Self::IdentityResolver;
    fn trust(&self) -> &Self::Trust;
    fn data(&self) -> &Self::Data;
    fn sync(&self) -> &Self::Sync;
}
