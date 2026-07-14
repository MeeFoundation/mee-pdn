//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::collections::HashMap;

use anyhow::Result;
use data_layer::{AuthorId, IdentityStores, SyncNode};
use pdn_types::{NodeId, PdnId};
use tokio::sync::Mutex;

use crate::connections::RuntimeConnectionsService;
use crate::data::RuntimeDataService;
use crate::identity::RuntimeIdentityService;
use crate::sync::RuntimeSyncService;

/// An operation addressed an identity this runtime does not host: `identity`
/// was neither created nor linked here. Downcast from the `anyhow::Error`
/// of identity-addressed service operations. Data-namespace operations
/// report the analogous [`data_layer::UnknownIssuer`] instead — namespaces
/// are registered by creation or import, not by hosting.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("identity not hosted on this runtime: {identity}")]
pub struct UnknownIdentity {
    /// The identity the operation addressed.
    pub identity: PdnId,
}

/// Shared runtime state: the node and the hosted identities.
///
/// One coarse lock: `SyncNode`'s registering operations take `&mut self`
/// and the services are driven concurrently (e.g. from HTTP handlers), so
/// all state sits behind a single async mutex. Fine at demo scale; note
/// that linking holds the lock for its whole wait.
pub(crate) struct State {
    pub(crate) node: SyncNode,
    /// The author for this runtime's data-namespace writes. The author
    /// dimension is not meaningful yet (see [`pdn_types::EntryInfo`]), so
    /// one per runtime suffices.
    pub(crate) author: AuthorId,
    /// The hosted identities' store handles, keyed by identity. Data-layer
    /// deliberately keeps no such list — store handles stay with the
    /// caller, and the runtime is that caller.
    pub(crate) identities: HashMap<PdnId, IdentityStores>,
}

impl State {
    /// The store set of `identity`, or [`UnknownIdentity`].
    pub(crate) fn hosted(&self, identity: PdnId) -> Result<&IdentityStores, UnknownIdentity> {
        self.identities
            .get(&identity)
            .ok_or(UnknownIdentity { identity })
    }
}

/// The embeddable runtime core: one running node plus the identities it
/// hosts. Spawn one per process (hosts) or several (in-process tests),
/// drive it through its services — [`identity`](Self::identity),
/// [`connections`](Self::connections), [`data`](Self::data),
/// [`sync`](Self::sync) — and shut it down.
///
/// The runtime is the single owner of node assembly: the `SyncNode` is
/// built at [`spawn`](Self::spawn) and nowhere else, so a future pairing
/// protocol handler (ADR-0011) threads through this one place.
pub struct Runtime {
    /// Cached at spawn; stable for the runtime's lifetime.
    node_id: NodeId,
    pub(crate) state: Mutex<State>,
}

impl Runtime {
    /// Spawn the node stack, hosting no identities yet.
    pub async fn spawn() -> Result<Self> {
        let node = SyncNode::spawn().await?;
        let author = node.create_author().await?;
        let node_id = node.node_id();
        Ok(Self {
            node_id,
            state: Mutex::new(State {
                node,
                author,
                identities: HashMap::new(),
            }),
        })
    }

    /// This runtime's node id (its endpoint id), stable from spawn to
    /// shutdown.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// The identity service: create identities here, link this runtime into
    /// existing ones.
    pub fn identity(&self) -> RuntimeIdentityService<'_> {
        RuntimeIdentityService::new(self)
    }

    /// The connections service: record and list hosted identities'
    /// connections.
    pub fn connections(&self) -> RuntimeConnectionsService<'_> {
        RuntimeConnectionsService::new(self)
    }

    /// The data service: entries by issuer and path, plus the interim
    /// whole-store ticket handover.
    pub fn data(&self) -> RuntimeDataService<'_> {
        RuntimeDataService::new(self)
    }

    /// The sync service: node id and hosted identities.
    pub fn sync(&self) -> RuntimeSyncService<'_> {
        RuntimeSyncService::new(self)
    }

    /// Shut the node down, closing the endpoint and all protocols.
    /// Consumes the runtime; services borrow it, so none can outlive this.
    pub async fn shutdown(self) -> Result<()> {
        self.state.into_inner().node.shutdown().await
    }
}
