//! The connections service: record and list a hosted identity's
//! connections.

use anyhow::Result;
use pdn_types::PdnId;

use crate::runtime::Runtime;

/// Recording and listing a hosted identity's connections, delegating to
/// that identity's connections store.
///
/// Manual recording is the current producer of connections; the
/// establishment dialogue (pairing, ADR-0011) becomes a producer in a later
/// change — the second implementation this trait anticipates.
#[allow(async_fn_in_trait)]
pub trait ConnectionsService {
    /// Record a connection between hosted `identity` and `peer`.
    async fn record(&self, identity: PdnId, peer: PdnId) -> Result<()>;

    /// List the current connections of hosted `identity`.
    async fn list(&self, identity: PdnId) -> Result<Vec<PdnId>>;
}

/// The production [`ConnectionsService`], backed by the runtime's
/// `data-layer` stack.
#[derive(Clone, Copy)]
pub struct RuntimeConnectionsService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeConnectionsService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }
}

impl ConnectionsService for RuntimeConnectionsService<'_> {
    async fn record(&self, identity: PdnId, peer: PdnId) -> Result<()> {
        let state = self.runtime.state.lock().await;
        state.hosted(identity)?.connections.connect(peer).await
    }

    async fn list(&self, identity: PdnId) -> Result<Vec<PdnId>> {
        let state = self.runtime.state.lock().await;
        state.hosted(identity)?.connections.list().await
    }
}
