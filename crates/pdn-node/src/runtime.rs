//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Result;
use data_layer::{
    AuthorId, ConnectionMetadata, NamespaceId, PrivateMetadataStore, SpawnOptions, SyncNode,
};
use pdn_types::{NodeId, PdnId};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

#[cfg(feature = "test-util")]
pub struct LinkAfterImportPause {
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(feature = "test-util")]
impl LinkAfterImportPause {
    pub async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

use crate::linking::LinkingLocalFailure;
use crate::{
    connections::RuntimeConnectionsService,
    data::RuntimeDataService,
    identity::RuntimeIdentityService,
    linking::{LinkingHandler, LINKING_ALPN},
    pairing::{PairingHandler, PendingInvites, PAIRING_ALPN},
    retraction::{spawn_retraction_consumer, RetractionEvent},
    sync::RuntimeSyncService,
};

/// How many retraction events the subscription buffers before a slow
/// subscriber starts losing the oldest — the broadcast channel's capacity.
const RETRACTION_EVENTS_CAPACITY: usize = 64;

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
    pub(crate) node: Arc<SyncNode>,
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
    /// Identities with a `link` currently in flight on this runtime — the
    /// linking analog of `grant_binders`: reserved right after the
    /// already-hosted check, in the same lock scope, so a second concurrent
    /// `link` toward the same not-yet-hosted identity refuses instead of
    /// racing the first to commit. Released on every exit path of
    /// `link_via_dialogue`, success and failure alike.
    pub(crate) linking_in_flight: HashSet<PdnId>,
    /// Pairs with an `establish` currently in flight, keyed by `(scanning
    /// identity, inviter)` — the establishment analog of
    /// `linking_in_flight`.
    pub(crate) establishing_in_flight: HashSet<(PdnId, PdnId)>,
    /// Asynchronous rollback work created by cancellation. Shutdown closes
    /// and joins this tracker before stopping the node underneath it.
    pub(crate) cleanup_tasks: TaskTracker,
    pub(crate) linking_failures: tokio::sync::broadcast::Sender<LinkingLocalFailure>,
    #[cfg(feature = "test-util")]
    pub(crate) link_after_import_pause: Option<Arc<LinkAfterImportPause>>,
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_pending_device_write: bool,
    /// The retraction-event surface: the verdict consumer sends, and
    /// [`Runtime::subscribe_retractions`] hands out receivers.
    pub(crate) retraction_events: tokio::sync::broadcast::Sender<RetractionEvent>,
}

impl State {
    /// The store set of `identity`, or [`UnknownIdentity`].
    pub(crate) fn hosted(&self, identity: PdnId) -> Result<&HostedIdentity, UnknownIdentity> {
        self.identities
            .get(&identity)
            .ok_or(UnknownIdentity { identity })
    }

    /// Whether `identity` is one of this runtime's own hosted identities —
    /// as opposed to a peer known only through a grant or a connection.
    pub(crate) fn is_hosted(&self, identity: PdnId) -> bool {
        self.identities.contains_key(&identity)
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
        // The provisional-write tracker recognizes this runtime's writes by
        // their author, and the runtime's consumer owns the verdict stream.
        node.track_writer_author(author);
        let verdicts = node
            .take_retraction_verdicts()
            .ok_or_else(|| anyhow::anyhow!("retraction verdict stream taken twice"))?;
        let node_id = node.node_id();
        let (retraction_events, _no_subscribers_yet) =
            tokio::sync::broadcast::channel(RETRACTION_EVENTS_CAPACITY);
        let (linking_failures, _no_failure_subscribers_yet) = tokio::sync::broadcast::channel(16);
        let state = Arc::new(Mutex::new(State {
            node: Arc::new(node),
            author,
            identities: HashMap::new(),
            pending_invites: PendingInvites::default(),
            pending_linking_invites: PendingInvites::default(),
            metadata_pairs: HashMap::new(),
            grant_binders: HashSet::new(),
            bound_grants: HashMap::new(),
            linking_in_flight: HashSet::new(),
            establishing_in_flight: HashSet::new(),
            cleanup_tasks: TaskTracker::new(),
            linking_failures,
            #[cfg(feature = "test-util")]
            link_after_import_pause: None,
            #[cfg(feature = "test-util")]
            fail_next_pending_device_write: false,
            retraction_events,
        }));
        spawn_retraction_consumer(Arc::downgrade(&state), verdicts, node_id);
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

    /// Subscribe to write-retraction events of this runtime's hosted
    /// identities: one event per retracted entry, carrying its full address
    /// — the host's hook for user-facing surfacing. A subscriber that lags
    /// past the buffer loses the oldest events, never blocks the runtime.
    pub async fn subscribe_retractions(&self) -> tokio::sync::broadcast::Receiver<RetractionEvent> {
        self.state.lock().await.retraction_events.subscribe()
    }

    pub async fn subscribe_linking_failures(
        &self,
    ) -> tokio::sync::broadcast::Receiver<LinkingLocalFailure> {
        self.state.lock().await.linking_failures.subscribe()
    }

    #[cfg(feature = "test-util")]
    pub async fn fail_next_pending_device_write_for_test(&self) {
        self.state.lock().await.fail_next_pending_device_write = true;
    }

    #[cfg(feature = "test-util")]
    pub async fn hold_state_lock_for_test(&self, duration: std::time::Duration) {
        let _state = self.state.lock().await;
        tokio::time::sleep(duration).await;
    }

    #[cfg(feature = "test-util")]
    pub async fn pause_next_link_after_import(&self) -> Arc<LinkAfterImportPause> {
        let pause = Arc::new(LinkAfterImportPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.link_after_import_pause = Some(Arc::clone(&pause));
        pause
    }

    /// Shut the node down, closing the endpoint and all protocols. Takes
    /// `&self`: no exclusive ownership is required — a clone held by a
    /// router, a handler, or another caller does not block this, and
    /// `SyncNode::shutdown`'s own idempotence makes a repeat call harmless.
    /// Clones the node handle and drops the state lock before awaiting the
    /// shutdown itself: `SyncNode::shutdown` now waits, bounded, for every
    /// in-flight pairing/linking accept to release its permit, and holding
    /// the coarse lock across that wait would stall every other operation
    /// on it — including an accept trying to take the very lock this
    /// shutdown is waiting on.
    pub async fn shutdown(&self) -> Result<()> {
        const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
        let (node, cleanup_tasks) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.node), state.cleanup_tasks.clone())
        };
        cleanup_tasks.close();
        let _ = tokio::time::timeout(CLEANUP_BUDGET, cleanup_tasks.wait()).await;
        node.shutdown().await
    }
}
