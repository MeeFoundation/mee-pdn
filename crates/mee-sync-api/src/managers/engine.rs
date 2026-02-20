use crate::{EntryInfo, SyncError, SyncHandle};
use futures_core::Stream;

use super::{DataManager, DelegationManager, NamespaceManager, NetworkManager, SessionManager};

// Composite split-responsibility surface
pub trait SyncEngine: Send + Sync {
    type Network: NetworkManager;
    type Namespaces: NamespaceManager;
    type Delegation: DelegationManager;
    type Sessions: SessionManager<Handle = Self::Handle>;
    type Data: DataManager<EntryStream = Self::EntryStream>;

    type Handle: SyncHandle;
    type EntryStream: Stream<Item = Result<EntryInfo, SyncError>> + Send + Unpin + 'static;

    fn network(&self) -> &Self::Network;
    fn namespaces(&self) -> &Self::Namespaces;
    fn delegation(&self) -> &Self::Delegation;
    fn sessions(&self) -> &Self::Sessions;
    fn data(&self) -> &Self::Data;
}
