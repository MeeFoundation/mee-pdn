//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use data_layer::{AuthorId, ConnectionMetadata, IdentityStores, SyncNode};
use pdn_types::{NodeId, PdnId};
use tokio::sync::Mutex;

use crate::connections::RuntimeConnectionsService;
use crate::data::RuntimeDataService;
use crate::identity::RuntimeIdentityService;
use crate::pairing::{PairingHandler, PendingInvites, PAIRING_ALPN};
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

/// How often shutdown re-checks that the pairing handler has released its
/// transient hold on the shared state.
const SHUTDOWN_RETRY: Duration = Duration::from_millis(10);

/// Shared runtime state: the node, the hosted identities, and the pairing
/// protocol's live bookkeeping.
///
/// One coarse lock: the hosted-identity map, the pending invites, and the
/// metadata-pair cache are mutated in place, so all state sits behind a
/// single async mutex the services (driven concurrently, e.g. from HTTP
/// handlers) serialize on. `SyncNode`'s own operations take `&self` — its
/// registry is interior-mutable — so they need no exclusive access. Coarse
/// on purpose: the state is small in-memory maps, and the writes under the
/// lock are local — no network wait — so splitting it buys nothing until
/// contention is measured rather than assumed. The exception is linking,
/// which holds the lock for its whole wait: a link in flight blocks every
/// other service call on this runtime. Establishment deliberately does not:
/// it takes the lock only for its local phases and releases it across the
/// network round-trip, so the accept side can take the lock to answer —
/// otherwise two runtimes establishing toward each other would deadlock.
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
    /// The pairing protocol's pending invites, keyed by secret bytes. In
    /// runtime memory on purpose: an invite is a live ceremony that does
    /// not survive a restart, and every operation on the set is a map
    /// operation under this one lock.
    pub(crate) pending_invites: PendingInvites,
    /// Connection metadata pairs opened on this runtime, keyed by
    /// `(hosted identity, counterparty)`: filled by establishment on the
    /// pairing device, and on demand from the directory's tickets
    /// everywhere else — a cache; the directory is the durable lookup.
    pub(crate) metadata_pairs: HashMap<(PdnId, PdnId), ConnectionMetadata>,
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
/// built at [`spawn`](Self::spawn) and nowhere else, and the pairing
/// protocol handler (ADR-0011) threads through this one place — built
/// before the node, registered at spawn through the data-layer assembly
/// slot, and handed the shared state right after.
pub struct Runtime {
    /// Cached at spawn; stable for the runtime's lifetime.
    node_id: NodeId,
    pub(crate) state: Arc<Mutex<State>>,
}

impl Runtime {
    /// Spawn the node stack, hosting no identities yet. The pairing handler
    /// registers on the node's endpoint here; its state slot is filled
    /// immediately after the node comes up, so by the time an invite can
    /// exist the handler is fully wired.
    pub async fn spawn() -> Result<Self> {
        let handler = PairingHandler::new();
        let slot = handler.slot();
        let node = SyncNode::spawn_with_protocols(vec![(PAIRING_ALPN.to_vec(), Box::new(handler))])
            .await?;
        let author = node.create_author().await?;
        let node_id = node.node_id();
        let state = Arc::new(Mutex::new(State {
            node,
            author,
            identities: HashMap::new(),
            pending_invites: PendingInvites::default(),
            metadata_pairs: HashMap::new(),
        }));
        slot.set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("pairing state slot filled twice"))?;
        Ok(Self { node_id, state })
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

    /// The connections service: establish hosted identities' connections
    /// (invite / establish), list them, and carry grants over the
    /// connections' metadata pairs.
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
        // The pairing handler holds the state only weakly, upgrading it per
        // connection for the accept's local verify-and-assemble alone (not
        // across the network reply), so sole ownership returns as soon as any
        // in-flight accept finishes that local work — a bounded wait, never
        // one that hangs on a dialer that completes the dialogue but does not
        // close.
        let mut shared = self.state;
        let state = loop {
            match Arc::try_unwrap(shared) {
                Ok(mutex) => break mutex.into_inner(),
                Err(still_shared) => {
                    shared = still_shared;
                    tokio::time::sleep(SHUTDOWN_RETRY).await;
                }
            }
        };
        state.node.shutdown().await
    }
}
