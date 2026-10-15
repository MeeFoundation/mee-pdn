//! The sync service: what this runtime is on the network and whom it
//! hosts.

use anyhow::Result;
use pdn_types::{NodeId, PdnId};

use crate::runtime::Runtime;

/// Reporting the runtime's node id and hosted identities.
#[allow(async_fn_in_trait)]
pub trait SyncService {
    fn node_id(&self) -> NodeId;

    /// Exactly the identities created or linked here, in no particular
    /// order.
    async fn hosted_identities(&self) -> Result<Vec<PdnId>>;

    /// An answer the storage itself gave: every other report here is
    /// in-memory bookkeeping, which a broken store leaves untouched.
    async fn check_storage(&self) -> Result<()>;
}

/// The production [`SyncService`].
#[derive(Clone, Copy)]
pub struct RuntimeSyncService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeSyncService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }
}

impl SyncService for RuntimeSyncService<'_> {
    fn node_id(&self) -> NodeId {
        self.runtime.node_id()
    }

    async fn hosted_identities(&self) -> Result<Vec<PdnId>> {
        let state = self.runtime.state.lock().await;
        Ok(state.identities.keys().copied().collect())
    }

    async fn check_storage(&self) -> Result<()> {
        let node = {
            let state = self.runtime.state.lock().await;
            std::sync::Arc::clone(&state.node)
        };
        node.check_stores().await
    }
}

impl RuntimeSyncService<'_> {
    #[cfg(feature = "test-util")]
    pub async fn dial_handle_for_test(&self) -> data_layer::DialHandle {
        self.runtime.state.lock().await.node.dial_handle()
    }

    /// The replicas `identity`'s registry tracks: what a create provisioned
    /// and what a cancelled link left, neither of which has a handle a
    /// scenario can check by.
    #[cfg(feature = "test-util")]
    pub async fn tracked_doc_count(&self, identity: PdnId) -> Result<usize> {
        let state = self.runtime.state.lock().await;
        state.node.tracked_doc_count(identity)
    }

    #[cfg(feature = "test-util")]
    pub async fn linking_in_flight_for_test(&self, identity: PdnId) -> bool {
        self.runtime
            .state
            .lock()
            .await
            .linking_in_flight
            .contains(&identity)
    }
}
