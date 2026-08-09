//! The sync service: what this runtime is on the network and whom it
//! hosts.

use anyhow::Result;
use pdn_types::{NodeId, PdnId};

use crate::runtime::Runtime;

/// Reporting the runtime's node id and hosted identities.
#[allow(async_fn_in_trait)]
pub trait SyncService {
    /// This runtime's node id — its endpoint id, stable for the runtime's
    /// lifetime.
    fn node_id(&self) -> NodeId;

    /// The identities this runtime hosts: exactly those created or linked
    /// on it, in no particular order.
    async fn hosted_identities(&self) -> Result<Vec<PdnId>>;
}

/// The production [`SyncService`], backed by the runtime's `data-layer`
/// stack.
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
}

impl RuntimeSyncService<'_> {
    /// The number of documents the node's reconcile pass currently tracks —
    /// pairing's own-replica has no hosted-identity-style handle a scenario
    /// can check by, so a cancelled or failed establishment attempt is
    /// asserted against this count instead. Behind the `test-util` feature
    /// and absent from every product build.
    #[cfg(feature = "test-util")]
    pub async fn tracked_doc_count(&self) -> Result<usize> {
        let state = self.runtime.state.lock().await;
        state.node.tracked_doc_count()
    }
}
