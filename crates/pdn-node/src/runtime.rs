//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use data_layer::{
    AuthorId, ConnectionMetadata, NamespaceId, PrivateMetadataStore, SpawnOptions, SyncNode,
};
use pdn_types::{NodeId, PdnId};
use tokio::sync::Mutex;

use crate::connections::RuntimeConnectionsService;
use crate::data::RuntimeDataService;
use crate::identity::RuntimeIdentityService;
use crate::linking::{LinkingHandler, LINKING_ALPN};
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

/// How often shutdown re-checks that a protocol handler has released its
/// transient hold on the shared state.
const SHUTDOWN_RETRY: Duration = Duration::from_millis(10);

/// A hosted identity's store handles — the runtime's own value, keyed by
/// identity in [`State::identities`]. Data-layer keeps no such list: store
/// handles stay with the caller, and the runtime is that caller.
#[derive(Debug)]
pub(crate) struct HostedIdentity {
    /// The identity's private-metadata directory: devices, tickets, and
    /// connections records.
    pub(crate) directory: PrivateMetadataStore,
}

/// Shared runtime state: the node, the hosted identities, and the live
/// bookkeeping of the runtime's two ceremonies (pairing and linking).
///
/// One coarse lock: the hosted-identity map, the pending invites, and the
/// metadata-pair cache are mutated in place, so all state sits behind a
/// single async mutex the services (driven concurrently, e.g. from HTTP
/// handlers) serialize on. `SyncNode`'s own operations take `&self` — its
/// registry is interior-mutable — so they need no exclusive access. Coarse
/// on purpose: the state is small in-memory maps, and the writes under the
/// lock are local — no network wait — so splitting it buys nothing until
/// contention is measured rather than assumed. Both ceremonies keep that
/// rule by taking the lock per phase and releasing it across every network
/// round-trip and wait — otherwise two runtimes running a ceremony toward
/// each other would deadlock, each holding its own lock while the peer's
/// accept side blocks on that same lock.
pub(crate) struct State {
    pub(crate) node: SyncNode,
    /// The author for this runtime's data-namespace writes. The author
    /// dimension carries no meaning (see [`pdn_types::EntryInfo`]), so one
    /// per runtime suffices.
    pub(crate) author: AuthorId,
    /// The hosted identities' store handles, keyed by identity: exactly
    /// those created or linked on this runtime.
    pub(crate) identities: HashMap<PdnId, HostedIdentity>,
    /// The pairing protocol's pending invites, keyed by secret bytes. In
    /// runtime memory on purpose: an invite is a live ceremony that does
    /// not survive a restart, and every operation on the set is a map
    /// operation under this one lock.
    pub(crate) pending_invites: PendingInvites,
    /// The linking protocol's pending invites — a second instance of the
    /// same set, deliberately separate from pairing's: a secret minted for
    /// one ceremony must never verify in the other.
    pub(crate) pending_linking_invites: PendingInvites,
    /// Connection metadata pairs opened on this runtime, keyed by
    /// `(hosted identity, counterparty)`: filled by establishment on the
    /// pairing device, by each hosted identity's connection armer as pair
    /// records replicate in from its other devices, and on demand from the
    /// directory's tickets — a cache; the directory is the durable lookup.
    pub(crate) metadata_pairs: HashMap<(PdnId, PdnId), ConnectionMetadata>,
    /// Pairs that currently have a grant binder running, keyed by
    /// `(hosted identity, counterparty)`. One binder per pair: the sweep
    /// inserts before spawning and the binder removes itself as it exits, so
    /// a pair whose replica was superseded gets a fresh binder on the next
    /// sweep rather than two watching different replicas.
    pub(crate) grant_binders: HashSet<(PdnId, PdnId)>,
    /// Data namespaces a grant binder imported, keyed by
    /// `(hosted identity, counterparty, issuer)` and holding the namespace
    /// the grant named when it was imported. Two jobs: a grant whose ticket
    /// still names this namespace is not re-imported every sweep, and a
    /// grant that disappears is forgotten again — bounded to what an binder
    /// itself brought in, so a namespace imported any other way is never
    /// dropped from under its owner.
    pub(crate) bound_grants: HashMap<(PdnId, PdnId, PdnId), NamespaceId>,
}

impl State {
    /// The store set of `identity`, or [`UnknownIdentity`].
    pub(crate) fn hosted(&self, identity: PdnId) -> Result<&HostedIdentity, UnknownIdentity> {
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
/// built at [`spawn`](Self::spawn) and nowhere else, and the runtime's two
/// protocol handlers — pairing (ADR-0011) and linking (ADR-0012) — thread
/// through this one place: built before the node, registered at spawn
/// through the data-layer assembly slot, and handed the shared state right
/// after.
pub struct Runtime {
    /// Cached at spawn; stable for the runtime's lifetime.
    node_id: NodeId,
    pub(crate) state: Arc<Mutex<State>>,
}

impl Runtime {
    /// Spawn the node stack, hosting no identities yet. The pairing and
    /// linking handlers register on the node's endpoint here; each handler
    /// gets its own state slot, filled immediately after the node comes up,
    /// so by the time an invite can exist both handlers are fully wired.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_with(SpawnOptions::default()).await
    }

    /// [`spawn`](Self::spawn), tuned by `options` — passed through to the
    /// node assembly.
    pub async fn spawn_with(options: SpawnOptions) -> Result<Self> {
        let pairing = PairingHandler::new();
        let pairing_slot = pairing.slot();
        let linking = LinkingHandler::new();
        let linking_slot = linking.slot();
        let node = SyncNode::spawn_with(
            vec![
                (PAIRING_ALPN.to_vec(), Box::new(pairing)),
                (LINKING_ALPN.to_vec(), Box::new(linking)),
            ],
            options,
        )
        .await?;
        let author = node.create_author().await?;
        let node_id = node.node_id();
        let state = Arc::new(Mutex::new(State {
            node,
            author,
            identities: HashMap::new(),
            pending_invites: PendingInvites::default(),
            pending_linking_invites: PendingInvites::default(),
            metadata_pairs: HashMap::new(),
            grant_binders: HashSet::new(),
            bound_grants: HashMap::new(),
        }));
        pairing_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("pairing state slot filled twice"))?;
        linking_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("linking state slot filled twice"))?;
        Ok(Self { node_id, state })
    }

    /// This runtime's node id (its endpoint id), stable from spawn to
    /// shutdown.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// The identity service: create identities here, mint linking invites
    /// for hosted ones, link this runtime into existing ones.
    pub fn identity(&self) -> RuntimeIdentityService<'_> {
        RuntimeIdentityService::new(self)
    }

    /// The connections service: establish hosted identities' connections
    /// (invite / establish), list them, and carry grants over the
    /// connections' metadata pairs.
    pub fn connections(&self) -> RuntimeConnectionsService<'_> {
        RuntimeConnectionsService::new(self)
    }

    /// The data service: entries by issuer and path, plus the whole-store
    /// ticket handover.
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
        // The protocol handlers hold the state only weakly, upgrading it
        // per connection for the accept's local verify-and-commit alone
        // (not across the network reply), so sole ownership returns as soon
        // as any in-flight accept finishes that local work — a bounded
        // wait, never one that hangs on a dialer that completes the
        // dialogue but does not close.
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
