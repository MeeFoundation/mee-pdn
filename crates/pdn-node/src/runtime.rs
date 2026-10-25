//! The runtime: single owner of the node assembly and the hosted-identity
//! set.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use data_layer::{AuthorId, NamespaceId, PrivateMetadataStore, SpawnOptions, SyncNode};
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

    pub(crate) fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
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

const MAX_SERVING_HALVES: usize = 1_048_576;
const MAX_SERVING_HALVES_U32: u32 = 1_048_576;

/// How long [`Runtime::shutdown`] waits for the ceremonies' serving halves
/// in flight before it stops the node anyway.
pub const SHUTDOWN_SERVING_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// One permit per serving half of either ceremony in flight, over either
/// transport. [`Runtime::shutdown`] drains them before the node stops, since
/// the node's stop shuts every engine down before its router, and a serving
/// half still running then writes into stopped stores. The count bounds
/// nothing: it gives the drain a fixed number to wait for.
#[derive(Debug, Clone)]
pub(crate) struct ServingHalves(Arc<tokio::sync::Semaphore>);

impl ServingHalves {
    pub(crate) fn new() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(MAX_SERVING_HALVES)))
    }

    /// `None` once the drain has run: the serving half refuses before it
    /// burns anything.
    pub(crate) async fn enter(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.0).acquire_owned().await.ok()
    }

    async fn drain(&self) {
        // Permits taken — all of them, or what a timed-out wait gathered —
        // are released only into the closed semaphore: released first, they
        // would go to a dialogue queued during the wait, which would burn
        // its secret on a stopping node.
        let acquire = self.0.acquire_many(MAX_SERVING_HALVES_U32);
        tokio::pin!(acquire);
        let held = tokio::time::timeout(SHUTDOWN_SERVING_BUDGET, &mut acquire).await;
        self.0.close();
        drop(held);
    }

    #[cfg(feature = "test-util")]
    fn any_in_flight(&self) -> bool {
        self.0.available_permits() < MAX_SERVING_HALVES
    }
}

#[cfg(feature = "test-util")]
pub struct CeremonyPause {
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(feature = "test-util")]
impl CeremonyPause {
    pub async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

use crate::linking::LinkingLocalFailure;
use crate::{
    ceremony::PendingInvites,
    identity::RuntimeIdentityService,
    linking::{LinkingHandler, LINKING_ALPN},
    pods::{JoinHandler, RuntimePodsService, POD_JOIN_ALPN},
    sync::RuntimeSyncService,
};

const LINKING_FAILURES_CAPACITY: usize = 16;

/// `identity` was neither created nor linked here. Downcast from the
/// `anyhow::Error` of identity-addressed operations.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("identity not hosted on this runtime: {identity}")]
pub struct UnknownIdentity {
    pub identity: PdnId,
}

/// A hosted identity's store handles and the author its writes carry;
/// data-layer keeps no such list.
#[derive(Debug)]
pub(crate) struct HostedIdentity {
    pub(crate) pms: PrivateMetadataStore,
    /// One author per hosted identity (ADR-0013), persisted with that
    /// identity's replicas.
    pub(crate) author: AuthorId,
}

/// Shared runtime state behind one coarse async mutex: small in-memory
/// maps, mutated in place under local writes only. Both ceremonies take the
/// lock per phase and release it across every network round-trip and wait
/// — otherwise two runtimes running a ceremony toward each other deadlock,
/// each holding its own lock while the peer's accept side blocks on it.
#[cfg_attr(feature = "test-util", allow(clippy::struct_excessive_bools))] // one-shot fault switches, each independent
pub(crate) struct State {
    pub(crate) node: Arc<SyncNode>,
    /// Exactly the identities created or linked here.
    pub(crate) identities: HashMap<PdnId, HostedIdentity>,
    /// In memory on purpose: an invite does not survive a restart.
    pub(crate) pending_linking_invites: PendingInvites,
    /// The pod join dialogue's, each secret minted for an identity and a
    /// pod. Separate from linking's: a secret minted for one ceremony must
    /// never verify in the other.
    pub(crate) pending_pod_invites: PendingInvites<(PdnId, pdn_types::PodId)>,
    /// Keyed by `(joining identity, pod)`.
    pub(crate) joining_in_flight: HashSet<(PdnId, pdn_types::PodId)>,
    /// Reserved in the same lock scope as the already-hosted check, so a
    /// concurrent `link` toward the same identity refuses instead of racing
    /// the first to commit.
    pub(crate) linking_in_flight: HashSet<PdnId>,
    /// Work a cancellation must not cut short — rollbacks, and a pod's
    /// create; shutdown joins it before stopping the node.
    pub(crate) cleanup_tasks: CleanupSupervisor,
    pub(crate) linking_failures: tokio::sync::broadcast::Sender<LinkingLocalFailure>,
    /// The in-process serving half enters here as `accept` does.
    pub(crate) serving_halves: ServingHalves,
    #[cfg(feature = "test-util")]
    pub(crate) link_after_import_pause: Option<Arc<CeremonyPause>>,
    /// A pause of the join dialogue's serving half, after it read the
    /// request and before it takes the state lock to verify and burn.
    #[cfg(feature = "test-util")]
    pub(crate) join_serve_pause: Option<Arc<CeremonyPause>>,
    /// A pause just before the linking commit point, where a scenario reads
    /// what a link has published before it commits — nothing.
    #[cfg(feature = "test-util")]
    pub(crate) link_before_commit_pause: Option<Arc<CeremonyPause>>,
    /// A pause just past the linking commit point, before the confirmation
    /// write — the one await a cancellation of a committed link can land on.
    #[cfg(feature = "test-util")]
    pub(crate) link_after_commit_pause: Option<Arc<CeremonyPause>>,
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_pending_device_write: bool,
    /// Ends the next join dialogue's serving half once the joined event is
    /// written, before the tickets go out: a reply lost.
    #[cfg(feature = "test-util")]
    pub(crate) drop_next_join_reply: bool,
    /// Fails the next join dialogue's write of the joined event, once its
    /// secret is burned, as a full disk would.
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_joined_write: bool,
    /// What the next join dialogue's serving half sends in place of the
    /// honest value, as a modified inviter would.
    #[cfg(feature = "test-util")]
    pub(crate) forged_join: Option<crate::pods::ForgedJoin>,
    /// A pause of the next join once its tickets and the PMS's entry are
    /// recorded, before it imports the stores.
    #[cfg(feature = "test-util")]
    pub(crate) join_import_pause: Option<Arc<CeremonyPause>>,
    /// Pods whose device statement every write fails in, as a process
    /// ended before it landed; a restart clears it.
    #[cfg(feature = "test-util")]
    pub(crate) failing_device_statements: HashSet<pdn_types::PodId>,
    /// Fails the next `create` where its PMS would be made — a step
    /// between provisioning an identity and hosting it, which a full disk is
    /// the product's reason to reach.
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_pms_create: bool,
    /// Fails the next commit point's hosting record, the write a full disk
    /// refuses there.
    #[cfg(feature = "test-util")]
    pub(crate) fail_next_hosting_record: bool,
    /// How long the armer waits for a PMS change before sweeping anyway —
    /// the spawn's reconcile interval.
    pub(crate) sweep_interval: std::time::Duration,
}

impl State {
    fn new(
        node: SyncNode,
        serving_halves: ServingHalves,
        sweep_interval: std::time::Duration,
    ) -> Self {
        let (linking_failures, _no_failure_subscribers_yet) =
            tokio::sync::broadcast::channel(LINKING_FAILURES_CAPACITY);
        Self {
            node: Arc::new(node),
            identities: HashMap::new(),
            pending_linking_invites: PendingInvites::default(),
            pending_pod_invites: PendingInvites::default(),
            joining_in_flight: HashSet::new(),
            linking_in_flight: HashSet::new(),
            cleanup_tasks: CleanupSupervisor::new(),
            linking_failures,
            serving_halves,
            #[cfg(feature = "test-util")]
            link_after_import_pause: None,
            #[cfg(feature = "test-util")]
            join_serve_pause: None,
            #[cfg(feature = "test-util")]
            link_before_commit_pause: None,
            #[cfg(feature = "test-util")]
            link_after_commit_pause: None,
            #[cfg(feature = "test-util")]
            fail_next_pending_device_write: false,
            #[cfg(feature = "test-util")]
            drop_next_join_reply: false,
            #[cfg(feature = "test-util")]
            fail_next_joined_write: false,
            #[cfg(feature = "test-util")]
            forged_join: None,
            #[cfg(feature = "test-util")]
            join_import_pause: None,
            #[cfg(feature = "test-util")]
            failing_device_statements: HashSet::new(),
            #[cfg(feature = "test-util")]
            fail_next_pms_create: false,
            #[cfg(feature = "test-util")]
            fail_next_hosting_record: false,
            sweep_interval,
        }
    }

    pub(crate) fn hosted(&self, identity: PdnId) -> Result<&HostedIdentity, UnknownIdentity> {
        self.identities
            .get(&identity)
            .ok_or(UnknownIdentity { identity })
    }

    /// The commit point of a create or a link: record `identity` as hosted
    /// with `pms` as its PMS. Called once the
    /// store set is provisioned and armed for session classification, and
    /// before the identity enters `identities`; a failure leaves no record,
    /// and the identity comes back at no start.
    pub(crate) async fn commit_hosting(&mut self, identity: PdnId, pms: NamespaceId) -> Result<()> {
        #[cfg(feature = "test-util")]
        if std::mem::take(&mut self.fail_next_hosting_record) {
            anyhow::bail!("recording the hosting failed for test");
        }
        self.node.record_hosting(identity, pms).await
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
    /// Cancelled as the stop begins, so an observation ends with no record
    /// rather than a `closed` for each store the stop drops.
    #[cfg(feature = "test-util")]
    stopping: tokio_util::sync::CancellationToken,
}

impl Runtime {
    /// On a directory-configured runtime, every identity whose subdirectory
    /// records its hosting is hosted again before the spawn returns, through
    /// the same tail `create` runs: the PMS opens from the replica the
    /// node holds, arms classification, and its armer's sweeps bring the
    /// rest back. No ceremony, no dial; an unreadable record stops
    /// the spawn, and a directory with none is a first start.
    pub async fn spawn(options: SpawnOptions) -> Result<Self> {
        let sweep_interval = options.reconcile_interval;
        let serving_halves = ServingHalves::new();
        let linking = LinkingHandler::new(serving_halves.clone());
        let linking_slot = linking.slot();
        let join = JoinHandler::new(serving_halves.clone());
        let join_slot = join.slot();
        let node = SyncNode::spawn_with(
            vec![
                (LINKING_ALPN.to_vec(), Box::new(linking)),
                (POD_JOIN_ALPN.to_vec(), Box::new(join)),
            ],
            options,
        )
        .await?;
        let node_id = node.node_id();
        // One fallible step: whatever fails here, the node is shut down
        // before the error leaves, or a retry on the same directory in this
        // process would meet its open databases.
        let prepared = async {
            let notices = node
                .take_pod_notices()
                .ok_or_else(|| anyhow::anyhow!("pod notice stream taken twice"))?;
            let recovered = recover_hosted_identities(&node).await?;
            anyhow::Ok((notices, recovered))
        }
        .await;
        let (notices, recovered) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                let _ = node.shutdown().await;
                return Err(err);
            }
        };

        let state = Arc::new(Mutex::new(State::new(node, serving_halves, sweep_interval)));
        {
            let mut guard = state.lock().await;
            for (identity, hosted, changes) in recovered {
                admit_hosted(&state, &mut guard, identity, hosted, changes);
            }
            // Every identity the directory records is hosted again by now.
            guard.node.start_blob_collection();
        }
        crate::pods::spawn_pod_notice_consumer(Arc::downgrade(&state), notices);
        linking_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("linking state slot filled twice"))?;
        join_slot
            .set(Arc::downgrade(&state))
            .map_err(|_already_filled| anyhow::anyhow!("join state slot filled twice"))?;
        Ok(Self {
            node_id,
            state,
            #[cfg(feature = "test-util")]
            stopping: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn identity(&self) -> RuntimeIdentityService<'_> {
        RuntimeIdentityService::new(self)
    }

    pub fn sync(&self) -> RuntimeSyncService<'_> {
        RuntimeSyncService::new(self)
    }

    pub fn pods(&self) -> RuntimePodsService<'_> {
        RuntimePodsService::new(self)
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
    pub async fn drop_next_join_reply_for_test(&self) {
        self.state.lock().await.drop_next_join_reply = true;
    }

    #[cfg(feature = "test-util")]
    pub async fn fail_next_joined_write_for_test(&self) {
        self.state.lock().await.fail_next_joined_write = true;
    }

    /// The next join this runtime serves sends `forged` in place of the
    /// honest value.
    #[cfg(feature = "test-util")]
    pub async fn forge_next_join_for_test(&self, forged: crate::pods::ForgedJoin) {
        self.state.lock().await.forged_join = Some(forged);
    }

    /// The membership view of `identity`'s replica of `pod`'s membership
    /// store, a tombstone's included.
    #[cfg(feature = "test-util")]
    pub async fn pod_membership_view_for_test(
        &self,
        identity: pdn_types::PdnId,
        pod: pdn_types::PodId,
    ) -> anyhow::Result<data_layer::MembershipView> {
        let node = Arc::clone(&self.state.lock().await.node);
        node.held_pod_membership_view_for_test(identity, pod).await
    }

    /// The record view over `identity`'s replica of `pod`'s record store,
    /// every entry it holds beside its verdict.
    #[cfg(feature = "test-util")]
    pub async fn pod_record_view_for_test(
        &self,
        identity: pdn_types::PdnId,
        pod: pdn_types::PodId,
    ) -> anyhow::Result<data_layer::RecordView> {
        let node = Arc::clone(&self.state.lock().await.node);
        node.pod_record_view(identity, pod).await
    }

    /// A subscription to the stores of `pod` `identity` holds here, both or
    /// a tombstone's membership store alone. [`UnknownIdentity`] for an
    /// identity not hosted, [`data_layer::UnknownPod`] for one holding no
    /// store of the pod.
    #[cfg(feature = "test-util")]
    pub async fn observe_pod_for_test(
        &self,
        identity: pdn_types::PdnId,
        pod: pdn_types::PodId,
    ) -> anyhow::Result<crate::observation::PodSubscription> {
        let node = {
            let state = self.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        let observation = node
            .observe_pod_for_test(identity, pod, crate::observation::STORE_CAPACITY)
            .await?;
        let sub = crate::observation::fresh_sub()?;
        Ok(crate::observation::subscribe(
            pod,
            observation,
            self.stopping.clone(),
            crate::observation::RECORD_BUFFER_BOUND,
            sub,
        ))
    }

    /// `join` with the subscription of `identity` to the pod opened between
    /// the import and the start of either store's sync. Answers both also
    /// when the catch-up outlasts the join's wait, the pod left recorded as
    /// `join` leaves it; a join refused or failed before then answers as
    /// `join` does and opens nothing.
    #[cfg(feature = "test-util")]
    pub async fn join_pod_observed_for_test(
        &self,
        identity: pdn_types::PdnId,
        invite: crate::PodInvite,
    ) -> anyhow::Result<(pdn_types::PodId, crate::observation::PodSubscription)> {
        let sub = crate::observation::fresh_sub()?;
        let (pod, stopping) = (invite.pod, self.stopping.clone());
        let (pod, subscription, waited) = self
            .pods()
            .join_with(identity, invite, move |node, tickets| async move {
                let (caught_up, observation) = node
                    .import_pod_observed_for_test(
                        identity,
                        pod,
                        tickets,
                        &[],
                        crate::observation::STORE_CAPACITY,
                    )
                    .await?;
                let subscription = crate::observation::subscribe(
                    pod,
                    observation,
                    stopping,
                    crate::observation::RECORD_BUFFER_BOUND,
                    sub,
                );
                Ok((caught_up, subscription))
            })
            .await?;
        // A catch-up outlasting the wait leaves the pod recorded as `join`
        // leaves it; any other failure of the wait is the join's.
        if let Err(err) = waited {
            if err.downcast_ref::<crate::CatchUpTimeout>().is_none() {
                return Err(err);
            }
        }
        Ok((pod, subscription))
    }

    /// Every entry of `identity`'s record store of `pod` as the record view
    /// holds it, with its verdict and, with `payloads`, its payload when
    /// present. Starts no session; refused as the record view refuses, a
    /// tombstone included.
    #[cfg(feature = "test-util")]
    pub async fn pod_record_listing_for_test(
        &self,
        identity: pdn_types::PdnId,
        pod: pdn_types::PodId,
        payloads: bool,
    ) -> anyhow::Result<Vec<crate::observation::ListedEntry>> {
        let node = {
            let state = self.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        Ok(node
            .pod_record_listing_for_test(identity, pod, payloads)
            .await?
            .into_iter()
            .map(|(entry, verdict, payload)| {
                crate::observation::ListedEntry::new(&entry, verdict, payload)
            })
            .collect())
    }

    /// Write `payload` at `key` of `identity`'s record store of `pod` under
    /// a fresh author, which no device statement lists: a forged entry.
    #[cfg(feature = "test-util")]
    pub async fn forge_pod_record_for_test(
        &self,
        identity: pdn_types::PdnId,
        pod: pdn_types::PodId,
        key: &[u8],
        payload: &[u8],
    ) -> anyhow::Result<()> {
        let node = {
            let state = self.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        let forger = node.create_author(identity).await?;
        node.write_pod_entry_as_for_test(
            identity,
            pod,
            data_layer::PodStore::Records,
            forger,
            key,
            payload,
        )
        .await
    }

    #[cfg(feature = "test-util")]
    pub async fn pause_next_join_import(&self) -> Arc<CeremonyPause> {
        let pause = Arc::new(CeremonyPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.join_import_pause = Some(Arc::clone(&pause));
        pause
    }

    /// Every device statement this runtime writes into `pod` fails from
    /// now on, until the runtime restarts.
    #[cfg(feature = "test-util")]
    pub async fn fail_device_statements_for_test(&self, pod: pdn_types::PodId) {
        self.state
            .lock()
            .await
            .failing_device_statements
            .insert(pod);
    }

    /// While `refuse`, every session on a pod's store `identity` is asked
    /// to serve here is refused, as by a device out of reach.
    #[cfg(feature = "test-util")]
    pub async fn refuse_pod_sessions_for_test(
        &self,
        identity: pdn_types::PdnId,
        refuse: bool,
    ) -> anyhow::Result<()> {
        self.state
            .lock()
            .await
            .node
            .refuse_pod_sessions_for_test(identity, refuse)
    }

    /// The pods `identity` holds on this node, each with whether its
    /// record store is held: `false` for a tombstone.
    #[cfg(feature = "test-util")]
    pub async fn pod_holdings_for_test(
        &self,
        identity: pdn_types::PdnId,
    ) -> anyhow::Result<Vec<(pdn_types::PodId, bool)>> {
        self.state.lock().await.node.pod_holdings(identity)
    }

    /// Every exchange of `pod`'s stores this node has running, held behind
    /// another's or due to redial, each named, for a failure message.
    #[cfg(feature = "test-util")]
    pub async fn pod_syncs_in_flight_report_for_test(
        &self,
        pod: pdn_types::PodId,
    ) -> anyhow::Result<String> {
        let node = Arc::clone(&self.state.lock().await.node);
        node.pod_syncs_in_flight_report_for_test(pod).await
    }

    /// The replicas `identity`'s store holds, tracked or not.
    #[cfg(feature = "test-util")]
    pub async fn held_replica_count_for_test(
        &self,
        identity: pdn_types::PdnId,
    ) -> anyhow::Result<usize> {
        let node = Arc::clone(&self.state.lock().await.node);
        node.held_replica_count(identity).await
    }

    #[cfg(feature = "test-util")]
    pub async fn fail_next_pms_create_for_test(&self) {
        self.state.lock().await.fail_next_pms_create = true;
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

    /// The runtime's dial side, for a scenario that holds connections of
    /// its own to a peer.
    #[cfg(feature = "test-util")]
    pub async fn dial_handle_for_test(&self) -> data_layer::DialHandle {
        self.state.lock().await.node.dial_handle()
    }

    /// [`SyncNode::sync_replica_store_for_test`](data_layer::SyncNode::sync_replica_store_for_test)
    /// for `identity`.
    #[cfg(feature = "test-util")]
    pub async fn sync_replica_store_for_test(
        &self,
        identity: pdn_types::PdnId,
    ) -> anyhow::Result<()> {
        let node = std::sync::Arc::clone(&self.state.lock().await.node);
        node.sync_replica_store_for_test(identity).await
    }

    /// [`SyncNode::own_entries_without_payload`](data_layer::SyncNode::own_entries_without_payload)
    /// for `identity`.
    #[cfg(feature = "test-util")]
    pub async fn own_entries_without_payload_for_test(
        &self,
        identity: pdn_types::PdnId,
    ) -> anyhow::Result<usize> {
        let node = std::sync::Arc::clone(&self.state.lock().await.node);
        node.own_entries_without_payload(identity).await
    }

    /// The public identity key in `identity`'s own PMS on this
    /// runtime; `None` until its payload has arrived.
    #[cfg(feature = "test-util")]
    pub async fn identity_public_key_for_test(
        &self,
        identity: pdn_types::PdnId,
    ) -> anyhow::Result<Option<[u8; 32]>> {
        let state = self.state.lock().await;
        let key = state.hosted(identity)?.pms.identity_key().await?;
        Ok(key.map(|key| key.public_key()))
    }

    #[cfg(feature = "test-util")]
    pub async fn serving_half_in_flight_for_test(&self) -> bool {
        self.state.lock().await.serving_halves.any_in_flight()
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
    pub async fn pause_next_link_after_import(&self) -> Arc<CeremonyPause> {
        let pause = Arc::new(CeremonyPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.link_after_import_pause = Some(Arc::clone(&pause));
        pause
    }

    #[cfg(feature = "test-util")]
    pub async fn pause_next_join_serve(&self) -> Arc<CeremonyPause> {
        let pause = Arc::new(CeremonyPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.join_serve_pause = Some(Arc::clone(&pause));
        pause
    }

    #[cfg(feature = "test-util")]
    pub async fn pause_next_link_before_commit(&self) -> Arc<CeremonyPause> {
        let pause = Arc::new(CeremonyPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.link_before_commit_pause = Some(Arc::clone(&pause));
        pause
    }

    #[cfg(feature = "test-util")]
    pub async fn pause_next_link_after_commit(&self) -> Arc<CeremonyPause> {
        let pause = Arc::new(CeremonyPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        self.state.lock().await.link_after_commit_pause = Some(Arc::clone(&pause));
        pause
    }

    /// Idempotent. The state lock is dropped before the drain: a serving
    /// half in flight may be waiting for that very lock. The host's own
    /// calls in flight — a `link`, a `join` — are not waited for: a host
    /// awaits them before it stops the runtime.
    pub async fn shutdown(&self) -> Result<()> {
        const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
        #[cfg(feature = "test-util")]
        self.stopping.cancel();
        let (node, cleanup_tasks, serving_halves) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.node),
                state.cleanup_tasks.clone(),
                state.serving_halves.clone(),
            )
        };
        // First: a serving half's own rollback lands in the cleanup tasks.
        serving_halves.drain().await;
        cleanup_tasks.close();
        let _ = tokio::time::timeout(CLEANUP_BUDGET, cleanup_tasks.wait()).await;
        node.shutdown().await
    }
}

type PmsChanges = Box<dyn futures_lite::Stream<Item = Result<()>> + Send + Unpin + 'static>;

/// The one step every path hosting an identity takes: it enters the hosted
/// set and its armer starts, so no hosted identity serves its PMS while
/// nothing opens its pods on this device.
pub(crate) fn admit_hosted(
    state: &Arc<Mutex<State>>,
    guard: &mut State,
    identity: PdnId,
    hosted: HostedIdentity,
    changes: impl futures_lite::Stream<Item = Result<()>> + Send + Unpin + 'static,
) {
    guard.identities.insert(identity, hosted);
    crate::armer::spawn_identity_armer(Arc::downgrade(state), identity, changes);
}

/// Host every identity whose subdirectory records its hosting. An
/// identity whose record names a PMS replica the store does not hold
/// is skipped, loudly, and the rest come back; one the store holds but
/// cannot open fails the start, because a runtime hosting less than its
/// records name would look healthy while refusing everything.
async fn recover_hosted_identities(
    node: &SyncNode,
) -> Result<Vec<(PdnId, HostedIdentity, PmsChanges)>> {
    let mut recovered: Vec<(PdnId, HostedIdentity, PmsChanges)> = Vec::new();
    // Each skip leaves its record where it is, so it repeats on every start
    // rather than being erased by one.
    for record in node.recorded_hosting()? {
        if !record.store_present {
            tracing::warn!(
                identity = %record.identity,
                pms = %record.pms,
                "a hosting record names an identity whose replica store is absent; the identity is not hosted"
            );
            continue;
        }
        // The identity's own half of the node comes up first: its store is
        // where its PMS replica lives.
        node.provision_identity(record.identity).await?;
        let opened = PrivateMetadataStore::open(node, record.identity, record.pms)
            .await
            .with_context(|| {
                format!(
                    "cannot recover hosted identity {}: its PMS replica did not open",
                    record.identity
                )
            })?;
        let Some(pms) = opened else {
            let _ = node.unhost_identity(record.identity).await;
            tracing::warn!(
                identity = %record.identity,
                pms = %record.pms,
                "a hosting record names a PMS replica this node's store does not hold; the identity is not hosted"
            );
            continue;
        };
        let changes = pms.changes().await?;
        let author = node.default_author(record.identity)?;
        node.host_identity(record.identity, &pms)?;
        recovered.push((
            record.identity,
            HostedIdentity { pms, author },
            Box::new(changes),
        ));
    }
    Ok(recovered)
}
