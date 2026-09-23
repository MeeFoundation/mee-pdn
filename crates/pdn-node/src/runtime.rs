//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use data_layer::{
    AuthorId, ConnectionMetadata, NamespaceId, PrivateMetadataStore, SpawnOptions, SyncNode,
};
use pdn_types::{NodeId, PdnId};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

#[derive(Clone)]
pub(crate) struct CleanupSupervisor {
    tasks: TaskTracker,
    runtime: tokio::runtime::Handle,
}

impl CleanupSupervisor {
    fn new() -> Self {
        Self {
            tasks: TaskTracker::new(),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub(crate) fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn_on(task, &self.runtime)
    }

    fn close(&self) {
        self.tasks.close();
    }

    async fn wait(&self) {
        self.tasks.wait().await;
    }
}

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

/// A slow subscriber past this loses the oldest events.
const RETRACTION_EVENTS_CAPACITY: usize = 64;
const LINKING_FAILURES_CAPACITY: usize = 16;

/// `identity` was neither created nor linked here. Downcast from the
/// `anyhow::Error` of identity-addressed operations; data-namespace
/// operations report [`data_layer::UnknownIssuer`] instead.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("identity not hosted on this runtime: {identity}")]
pub struct UnknownIdentity {
    pub identity: PdnId,
}

/// A hosted identity's store handles and the author its writes carry;
/// data-layer keeps no such list.
#[derive(Debug)]
pub(crate) struct HostedIdentity {
    pub(crate) directory: PrivateMetadataStore,
    /// One author per hosted identity (ADR-0013), persisted with that
    /// identity's replicas.
    pub(crate) author: AuthorId,
}

/// Shared runtime state behind one coarse async mutex: small in-memory
/// maps, mutated in place under local writes only. Both ceremonies take the
/// lock per phase and release it across every network round-trip and wait
/// — otherwise two runtimes running a ceremony toward each other deadlock,
/// each holding its own lock while the peer's accept side blocks on it.
pub(crate) struct State {
    pub(crate) node: Arc<SyncNode>,
    /// Exactly the identities created or linked here.
    pub(crate) identities: HashMap<PdnId, HostedIdentity>,
    /// In memory on purpose: an invite does not survive a restart.
    pub(crate) pending_invites: PendingInvites,
    /// Separate from pairing's: a secret minted for one ceremony must never
    /// verify in the other.
    pub(crate) pending_linking_invites: PendingInvites,
    /// A cache keyed by `(hosted identity, counterparty)`; the directory is
    /// the durable lookup.
    pub(crate) metadata_pairs: HashMap<(PdnId, PdnId), ConnectionMetadata>,
    /// One binder per pair: the sweep inserts before spawning and the
    /// binder removes itself as it exits.
    pub(crate) grant_binders: HashSet<(PdnId, PdnId)>,
    /// What each binder imported, keyed by `(hosted identity, counterparty,
    /// issuer)`: bounds the unbind to what a binder itself brought in.
    pub(crate) bound_grants: HashMap<(PdnId, PdnId, PdnId), NamespaceId>,
    /// Reserved in the same lock scope as the already-hosted check, so a
    /// concurrent `link` toward the same identity refuses instead of racing
    /// the first to commit.
    pub(crate) linking_in_flight: HashSet<PdnId>,
    /// Keyed by `(scanning identity, inviter)`.
    pub(crate) establishing_in_flight: HashSet<(PdnId, PdnId)>,
    /// Rollback work created by cancellation; shutdown joins it before
    /// stopping the node.
    pub(crate) cleanup_tasks: CleanupSupervisor,
    pub(crate) linking_failures: tokio::sync::broadcast::Sender<LinkingLocalFailure>,
    #[cfg(feature = "test-util")]
    pub(crate) pairing_in_flight: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "test-util")]
    pub(crate) link_after_import_pause: Option<Arc<LinkAfterImportPause>>,
    /// A pause just before the linking commit point, where a scenario reads
    /// what a link has published before it commits — nothing.
    #[cfg(feature = "test-util")]
    pub(crate) link_before_commit_pause: Option<Arc<LinkAfterImportPause>>,
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_pending_device_write: bool,
    /// Fails the next `create` where its directory would be made — the one
    /// step between provisioning an identity and hosting it, which a full
    /// disk is the product's reason to reach.
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_directory_create: bool,
    /// Fails the next commit point's hosting record, the write a full disk
    /// refuses there.
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_hosting_record: bool,
    /// `Some(n)` fails every pair arming and counts the failures. Sticky:
    /// the armer retries every sweep, and the count is the positive control
    /// for "repeated attempts leave nothing open".
    #[cfg(feature = "test-util")]
    pub(crate) pair_arm_failures: Option<usize>,
    /// How long a connection armer waits for a directory change before
    /// sweeping anyway — the spawn's reconcile interval.
    pub(crate) sweep_interval: std::time::Duration,
    pub(crate) retraction_events: tokio::sync::broadcast::Sender<RetractionEvent>,
}

impl State {
    pub(crate) fn hosted(&self, identity: PdnId) -> Result<&HostedIdentity, UnknownIdentity> {
        self.identities
            .get(&identity)
            .ok_or(UnknownIdentity { identity })
    }

    /// The commit point of a create or a link: record `identity` as hosted
    /// with `directory` as its private metadata directory. Called after the
    /// store set is provisioned and before the identity is hosted; a
    /// failure leaves no record, and the identity comes back at no start.
    pub(crate) async fn commit_hosting(
        &mut self,
        identity: PdnId,
        directory: NamespaceId,
    ) -> Result<()> {
        #[cfg(feature = "test-util")]
        if std::mem::take(&mut self.fail_next_hosting_record) {
            anyhow::bail!("recording the hosting failed for test");
        }
        self.node.record_hosting(identity, directory).await
    }
}

/// The embeddable runtime core: one running node plus the identities it
/// hosts, driven through its services. The single owner of node assembly:
/// the two protocol handlers are built before the node, registered at spawn
/// through the data-layer assembly slot, and handed the shared state right
/// after.
pub struct Runtime {
    node_id: NodeId,
    pub(crate) state: Arc<Mutex<State>>,
}

impl Runtime {
    /// On a directory-configured runtime, every identity whose subdirectory
    /// records its hosting is hosted again before the spawn returns, through
    /// the same tail `create` runs: the directory opens from the replica the
    /// node holds, arms classification, and its connection armer's sweeps
    /// bring the rest back. No ceremony, no dial; an unreadable record stops
    /// the spawn, and a directory with none is a first start.
    pub async fn spawn(options: SpawnOptions) -> Result<Self> {
        let sweep_interval = options.reconcile_interval;
        let pairing = PairingHandler::new();
        let pairing_slot = pairing.slot();
        #[cfg(feature = "test-util")]
        let pairing_in_flight = pairing.in_flight_probe();
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
        let node_id = node.node_id();
        // One fallible step: whatever fails here, the node is shut down
        // before the error leaves, or a retry on the same directory in this
        // process would meet its open databases.
        let prepared = async {
            let verdicts = node
                .take_retraction_verdicts()
                .ok_or_else(|| anyhow::anyhow!("retraction verdict stream taken twice"))?;
            let (identities, armers) = recover_hosted_identities(&node).await?;
            anyhow::Ok((verdicts, identities, armers))
        }
        .await;
        let (verdicts, identities, armers) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                let _ = node.shutdown().await;
                return Err(err);
            }
        };

        let (retraction_events, _no_subscribers_yet) =
            tokio::sync::broadcast::channel(RETRACTION_EVENTS_CAPACITY);
        let (linking_failures, _no_failure_subscribers_yet) =
            tokio::sync::broadcast::channel(LINKING_FAILURES_CAPACITY);
        let state = Arc::new(Mutex::new(State {
            node: Arc::new(node),
            identities,
            pending_invites: PendingInvites::default(),
            pending_linking_invites: PendingInvites::default(),
            metadata_pairs: HashMap::new(),
            grant_binders: HashSet::new(),
            bound_grants: HashMap::new(),
            linking_in_flight: HashSet::new(),
            establishing_in_flight: HashSet::new(),
            cleanup_tasks: CleanupSupervisor::new(),
            linking_failures,
            #[cfg(feature = "test-util")]
            pairing_in_flight,
            #[cfg(feature = "test-util")]
            link_after_import_pause: None,
            #[cfg(feature = "test-util")]
            link_before_commit_pause: None,
            #[cfg(feature = "test-util")]
            fail_next_pending_device_write: false,
            #[cfg(feature = "test-util")]
            fail_next_directory_create: false,
            #[cfg(feature = "test-util")]
            fail_next_hosting_record: false,
            #[cfg(feature = "test-util")]
            pair_arm_failures: None,
            retraction_events,
            sweep_interval,
        }));
        for (identity, changes) in armers {
            crate::connections::spawn_connection_armer(Arc::downgrade(&state), identity, changes);
        }
        spawn_retraction_consumer(Arc::downgrade(&state), verdicts, node_id);
        pairing_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("pairing state slot filled twice"))?;
        linking_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("linking state slot filled twice"))?;
        Ok(Self { node_id, state })
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn identity(&self) -> RuntimeIdentityService<'_> {
        RuntimeIdentityService::new(self)
    }

    pub fn connections(&self) -> RuntimeConnectionsService<'_> {
        RuntimeConnectionsService::new(self)
    }

    pub fn data(&self) -> RuntimeDataService<'_> {
        RuntimeDataService::new(self)
    }

    pub fn sync(&self) -> RuntimeSyncService<'_> {
        RuntimeSyncService::new(self)
    }

    /// One event per retracted entry — the host's hook for user-facing
    /// surfacing. A lagging subscriber loses the oldest events, never
    /// blocks the runtime.
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
    pub async fn fail_next_directory_create_for_test(&self) {
        self.state.lock().await.fail_next_directory_create = true;
    }

    #[cfg(feature = "test-util")]
    pub async fn fail_next_hosting_record_for_test(&self) {
        self.state.lock().await.fail_next_hosting_record = true;
    }

    /// The identities the node has a half of — its own engine, store and
    /// author. Wider than what the runtime hosts: an identity is
    /// provisioned first and hosted at the commit point, and a create that
    /// fails in between must leave neither.
    #[cfg(feature = "test-util")]
    pub async fn provisioned_identities_for_test(&self) -> anyhow::Result<Vec<pdn_types::PdnId>> {
        self.state.lock().await.node.hosted_identities()
    }

    /// Fail every pair arming from now on.
    #[cfg(feature = "test-util")]
    pub async fn fail_pair_arm_for_test(&self) {
        self.state.lock().await.pair_arm_failures = Some(0);
    }

    /// The positive control for "nothing accumulated".
    #[cfg(feature = "test-util")]
    pub async fn pair_arm_failures_for_test(&self) -> usize {
        self.state.lock().await.pair_arm_failures.unwrap_or(0)
    }

    #[cfg(feature = "test-util")]
    pub async fn pairing_accept_in_flight_for_test(&self) -> bool {
        self.state
            .lock()
            .await
            .pairing_in_flight
            .available_permits()
            < crate::pairing::MAX_CONCURRENT_ESTABLISHMENTS
    }

    #[cfg(feature = "test-util")]
    pub async fn hold_state_lock_for_test(
        &self,
        acquired: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        let _state = self.state.lock().await;
        acquired.notify_one();
        release.notified().await;
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

    #[cfg(feature = "test-util")]
    pub async fn pause_next_link_before_commit(&self) -> Arc<LinkAfterImportPause> {
        let pause = Arc::new(LinkAfterImportPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.link_before_commit_pause = Some(Arc::clone(&pause));
        pause
    }

    /// Idempotent. The state lock is dropped before the shutdown is
    /// awaited: `SyncNode::shutdown` waits for in-flight accepts, and one of
    /// them may be trying to take that very lock.
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

type DirectoryChanges = Box<dyn futures_lite::Stream<Item = Result<()>> + Send + Unpin + 'static>;

/// Host every identity whose subdirectory records its hosting. An
/// identity whose record names a directory replica the store does not hold
/// is skipped, loudly, and the rest come back; one the store holds but
/// cannot open fails the start, because a runtime hosting less than its
/// records name would look healthy while refusing everything.
async fn recover_hosted_identities(
    node: &SyncNode,
) -> Result<(
    HashMap<PdnId, HostedIdentity>,
    Vec<(PdnId, DirectoryChanges)>,
)> {
    let mut identities = HashMap::new();
    let mut armers: Vec<(PdnId, DirectoryChanges)> = Vec::new();
    // Each skip leaves its record where it is, so it repeats on every start
    // rather than being erased by one.
    for record in node.recorded_hosting()? {
        if !record.store_present {
            tracing::warn!(
                identity = %record.identity,
                directory = %record.directory,
                "a hosting record names an identity whose replica store is absent; the identity is not hosted"
            );
            continue;
        }
        // The identity's own half of the node comes up first: its store is
        // where its directory replica lives.
        node.provision_identity(record.identity).await?;
        let opened = PrivateMetadataStore::open(node, record.identity, record.directory)
            .await
            .with_context(|| {
                format!(
                    "cannot recover hosted identity {}: its directory replica did not open",
                    record.identity
                )
            })?;
        let Some(directory) = opened else {
            let _ = node.unhost_identity(record.identity).await;
            tracing::warn!(
                identity = %record.identity,
                directory = %record.directory,
                "a hosting record names a directory replica this node's store does not hold; the identity is not hosted"
            );
            continue;
        };
        let changes = directory.changes().await?;
        let author = node.default_author(record.identity)?;
        node.host_identity(record.identity, &directory)?;
        identities.insert(record.identity, HostedIdentity { directory, author });
        armers.push((record.identity, Box::new(changes)));
    }
    Ok((identities, armers))
}
