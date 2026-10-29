//! The handle an application holds: one handle, one node.
//!
//! Bring-up and stop are explicit acts. A handle refuses a second bring-up
//! of its own rather than replacing its node, and a stop is safe to repeat.
//! The constraint is on the handle rather than on the process, so a test
//! binary holding 2 handles against 2 runtimes stays possible while an
//! application cannot grow a node set behind one — an arrangement that
//! could not show the act it would exist for, since a device that goes away
//! takes every runtime co-located with it.
//!
//! The facade owns the asynchronous runtime the operations need: a foreign
//! caller brings none, and every exported call is awaited rather than
//! blocking a caller's thread until the network answers. Where a caller's
//! continuation resumes is the caller's own affair.
//!
//! Storage is named by the embedder at bring-up, because the embedder is
//! the one that knows the sandbox it may write in. Nothing here derives a
//! directory or reads one from the environment.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use pdn_node::{
    Connectivity, IdentityService as _, LinkingPayload, PodInvite, PodsService as _, Runtime,
    SpawnOptions, SyncService as _,
};

use crate::{
    error::{self, PdnError},
    payload,
    shapes::{
        self, InviteState, PodMemberInfo, PodMembershipAct, PodOperation, PodRecord, PodRecordKind,
    },
    streams::{self, RecordReader, RecordWriter},
};

/// What a handle holds. `ComingUp` is a state of its own so 2 concurrent
/// bring-ups cannot both reach the spawn.
pub(crate) enum State {
    Down,
    ComingUp(BringUp),
    Up(Arc<Runtime>),
}

/// One bring-up in flight, told from a later one by identity: its task holds
/// the lock until its node is installed or shut down, which is what a stop
/// that overtakes it waits for.
pub(crate) type BringUp = Arc<tokio::sync::Mutex<()>>;

/// Whether `state` is still the bring-up `ours` claimed; a stop and a later
/// bring-up may both have come between.
pub(crate) fn claimed(state: &State, ours: &BringUp) -> bool {
    matches!(state, State::ComingUp(held) if Arc::ptr_eq(held, ours))
}

/// One node, reached through one handle.
///
/// Every call names the identity or the issuer it acts for, and the runtime
/// keeps those apart; the handle holds no notion of a current identity, so
/// nothing here can act as one identity while a screen believes it is
/// acting as another.
#[derive(uniffi::Object)]
pub struct PdnNode {
    /// The asynchronous runtime the facade owns. Taken on drop, so the
    /// teardown of a handle released without a stop runs on a thread of its
    /// own rather than on the thread that let it go.
    tokio: Mutex<Option<tokio::runtime::Runtime>>,
    /// Shared with the bring-up's own task, which is what installs a node:
    /// a caller whose call is cancelled — a cancelled coroutine, an
    /// abandoned task — cannot leave the state mid-transition, because the
    /// transition does not belong to the caller's future.
    state: Arc<Mutex<State>>,
    /// The test build's surface, served or not (`test_surface`).
    #[cfg(feature = "observation")]
    surface: Arc<crate::test_surface::Shared>,
}

/// Writes the runtime's diagnostics to stderr, once; the embedder routes
/// stderr to the platform's log, or installs a subscriber of its own first
/// and keeps it.
///
/// What the error table calls an unrecognized failure carries no cause across
/// the boundary — the cause chain stays in the log — so without this the one
/// refusal a person cannot act on is also the one nobody can diagnose.
fn install_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(err) = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init()
        {
            tracing::debug!("a subscriber is installed already: {err}");
        }
    });
}

#[uniffi::export]
impl PdnNode {
    /// A handle with no node behind it yet.
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, PdnError> {
        install_log();
        // The prefix is what a sampler in the same process (the
        // `observation` build's) tells the node's threads by; the name
        // changes nothing else, so every build carries it.
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .thread_name("pdn-node")
            .enable_all()
            .build()
            .map_err(|err| {
                tracing::error!("the facade's asynchronous runtime did not start: {err}");
                PdnError::Internal
            })?;
        Ok(Arc::new(Self {
            tokio: Mutex::new(Some(tokio)),
            state: Arc::new(Mutex::new(State::Down)),
            #[cfg(feature = "observation")]
            surface: Arc::default(),
        }))
    }

    /// Bring the node up on `storage_dir`, reconciling every
    /// `reconcile_interval_secs`.
    ///
    /// The directory is the node's only copy of what it holds: its
    /// replicas, its payloads and its own key live there, and a node
    /// brought up on it again is the same node. A directory belongs to one
    /// running node, so a bring-up on one another node holds is refused as
    /// that rather than as a corrupt store.
    ///
    /// The interval is named rather than defaulted, because a short one
    /// costs radio wakeups and battery and the choice belongs to a host
    /// that knows what it is running. It bounds a write reaching another
    /// device of the same identity and a linking catch-up.
    ///
    /// Installing the node is the spawn's own last act rather than this
    /// call's, so a caller that abandons the call — a cancelled coroutine,
    /// a task that goes away — leaves the handle either up or down and
    /// never mid-transition. A stop that overtakes a bring-up wins: the
    /// node that had just come up is shut down again, and the bring-up
    /// reports the node as not up rather than handing back one the caller
    /// has already asked to stop.
    pub async fn bring_up(
        &self,
        storage_dir: String,
        reconcile_interval_secs: u64,
    ) -> Result<(), PdnError> {
        // A phone moves between networks, and a peer reached only at an
        // address published on the last one is not reached at all — which is
        // what `for_product` names, relays and address lookup included.
        self.bring_up_on(
            storage_dir,
            reconcile_interval_secs,
            Connectivity::RelaysAndAddressLookup,
        )
        .await
    }

    /// Stop the node. Safe to repeat: a stop with nothing up is a no-op
    /// rather than a failure, and a second stop during the first waits for
    /// it. A bring-up during a stop is refused as a node already up.
    ///
    /// What a stop costs is availability, not what the node holds. A peer
    /// that needed data while the node was down did not get it, and the
    /// identities and pods are found again on the next bring-up. What does
    /// not come back is work in flight — an invite minted and not consumed,
    /// a ceremony interrupted.
    pub async fn stop(&self) -> Result<(), PdnError> {
        let (ours, held) = fresh_bring_up().map_err(|err| {
            tracing::error!("a stop's lock was not free: {err}");
            PdnError::Internal
        })?;
        let previous = {
            let mut state = self.state()?;
            if matches!(*state, State::Down) {
                return Ok(());
            }
            // The handle stays between its states until the previous owner
            // of the directory is gone: a bring-up meanwhile would find it
            // held, and a stop meanwhile waits on this one's lock.
            std::mem::replace(&mut *state, State::ComingUp(Arc::clone(&ours)))
        };
        let state = Arc::clone(&self.state);
        self.on_node(async move {
            let _held = held;
            let stopped = match previous {
                State::Up(runtime) => runtime.shutdown().await,
                // The overtaken bring-up shuts its node down before its lock
                // goes; a stop under way does the same.
                State::ComingUp(theirs) => {
                    let _ended = theirs.lock().await;
                    Ok(())
                }
                State::Down => Ok(()),
            };
            if let Ok(mut guard) = state.lock() {
                if claimed(&guard, &ours) {
                    *guard = State::Down;
                }
            }
            stopped
        })
        .await
    }

    /// The node id this node answers as — the same one after a bring-up on
    /// the same directory, because the key that decides it lives there.
    pub async fn node_id(&self) -> Result<String, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move { Ok(runtime.sync().node_id().to_string()) })
            .await
    }

    /// Create an identity on this node — its first device.
    ///
    /// An identity is a placeholder value with no key material behind it,
    /// so nothing binds one to a person.
    pub async fn create_identity(&self) -> Result<String, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move { Ok(runtime.identity().create().await?.to_string()) })
            .await
    }

    /// The identities this node hosts.
    pub async fn hosted_identities(&self) -> Result<Vec<String>, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move {
            let hosted = runtime.sync().hosted_identities().await?;
            Ok(hosted.iter().map(ToString::to_string).collect())
        })
        .await
    }

    /// Mint a payload for another device to join `identity`.
    ///
    /// The payload carries a live one-time secret: nothing in it grants
    /// durable access, which is why it is safe to show to a camera and
    /// unsafe to leave on a table. Whoever photographs it can consume it in
    /// the intended device's place until the secret burns or expires.
    pub async fn mint_linking_payload(
        &self,
        identity: String,
        lifetime_secs: Option<u64>,
    ) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let lifetime = shapes::lifetime(lifetime_secs)?;
        let runtime = self.runtime()?;
        let minted = self
            .on_node(async move { runtime.identity().linking_invite(identity, lifetime).await })
            .await?;
        payload::encode(&minted)
    }

    /// What became of a linking payload this node minted (device-linking
    /// spec); `Used` names the newcomer's node id. Writes nothing.
    pub async fn linking_payload_state(&self, code: String) -> Result<InviteState, PdnError> {
        let payload: LinkingPayload = payload::decode(&code)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.identity().linking_invite_state(&payload).await })
            .await
            .map(InviteState::from)
    }

    /// Join the identity a linking payload names, as a device of it.
    ///
    /// `budget_secs` bounds the whole act: the dialogue spends from it
    /// first and the catch-up takes what remains. A catch-up that does not
    /// finish inside it reports its own failure and leaves nothing behind.
    pub async fn consume_linking_payload(
        &self,
        code: String,
        budget_secs: u64,
    ) -> Result<(), PdnError> {
        let payload: LinkingPayload = payload::decode(&code)?;
        let budget = shapes::duration(budget_secs, "budget")?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.identity().link(payload, budget).await })
            .await
    }

    /// Create a pod with `identity` as its first owner.
    pub async fn create_pod(&self, identity: String) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().create(identity).await })
            .await
            .map(|pod| pod.to_string())
    }

    /// The pods `identity` holds, by its PMS.
    pub async fn pods(&self, identity: String) -> Result<Vec<String>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let pods = runtime.pods().list(identity).await?;
            Ok(pods.iter().map(|info| info.id.to_string()).collect())
        })
        .await
    }

    /// The pod's current members as `identity`'s replica folds them.
    pub async fn pod_members(
        &self,
        identity: String,
        pod: String,
    ) -> Result<Vec<PodMemberInfo>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let members = runtime.pods().members(identity, pod).await?;
            Ok(members.into_iter().map(PodMemberInfo::from).collect())
        })
        .await
    }

    /// Mint an invitation into `pod`. Its exposure is the one
    /// [`Self::mint_linking_payload`] describes.
    pub async fn mint_pod_invite(
        &self,
        identity: String,
        pod: String,
        lifetime_secs: Option<u64>,
    ) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let lifetime = shapes::lifetime(lifetime_secs)?;
        let runtime = self.runtime()?;
        let minted = self
            .on_node(async move { runtime.pods().invite(identity, pod, lifetime).await })
            .await?;
        payload::encode(&minted)
    }

    /// What became of a pod invite this node minted for `identity` (pods
    /// spec); `Used` names the joiner. Writes nothing.
    pub async fn pod_invite_state(
        &self,
        identity: String,
        code: String,
    ) -> Result<InviteState, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let invite: PodInvite = payload::decode(&code)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().invite_state(identity, &invite).await })
            .await
            .map(InviteState::from)
    }

    /// Join the pod an invitation names, as a plain member. Answers once
    /// both stores caught up; [`PdnError::CatchUpTimedOut`] leaves the pod
    /// recorded, its catch-up continuing.
    pub async fn join_pod(&self, identity: String, code: String) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let invite: PodInvite = payload::decode(&code)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().join(identity, invite).await })
            .await
            .map(|pod| pod.to_string())
    }

    /// Write a membership act; a refused one writes nothing.
    pub async fn pod_act(
        &self,
        identity: String,
        pod: String,
        act: PodMembershipAct,
    ) -> Result<(), PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let act = act.parse()?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().act(identity, pod, act).await })
            .await
    }

    /// Place a record under `identity`'s own name at a fresh id.
    pub async fn put_record(
        &self,
        identity: String,
        pod: String,
        kind: PodRecordKind,
        payload: Vec<u8>,
    ) -> Result<PodRecord, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        shapes::entry_payload(kind, &payload)?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            runtime
                .pods()
                .put_record(identity, pod, kind.into(), &payload)
                .await
        })
        .await
        .map(PodRecord::from)
    }

    /// Append an operation to a mergeable-document, whoever's name it sits
    /// under.
    pub async fn append_op(
        &self,
        identity: String,
        pod: String,
        record: PodRecord,
        op: Vec<u8>,
    ) -> Result<(), PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let record = shapes::record(&record)?;
        shapes::entry_payload(PodRecordKind::MergeableDocument, &op)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().append_op(identity, pod, record, &op).await })
            .await
    }

    /// A claim's or an immutable-document's payload. `None` covers a record
    /// that has not arrived yet as well as one that does not exist.
    pub async fn read_record(
        &self,
        identity: String,
        pod: String,
        record: PodRecord,
    ) -> Result<Option<Vec<u8>>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let record = shapes::record(&record)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.pods().read(identity, pod, record).await })
            .await
    }

    /// Place a record whose payload the host writes in pieces; the writer's
    /// `finish` answers the record, and nothing is placed before it.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)] // asynchronous as every exported call is; it starts the placing
    pub async fn put_record_streamed(
        &self,
        identity: String,
        pod: String,
        kind: PodRecordKind,
    ) -> Result<Arc<RecordWriter>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let runtime = self.runtime()?;
        let (pieces, payload) = streams::pieces();
        let placing = self.handle()?.spawn(async move {
            runtime
                .pods()
                .put_record_streamed(identity, pod, kind.into(), payload)
                .await
        });
        Ok(Arc::new(RecordWriter::new(kind, pieces, placing)))
    }

    /// `read_record` as a reader, `None` as `read_record` answers it.
    pub async fn read_record_streamed(
        &self,
        identity: String,
        pod: String,
        record: PodRecord,
    ) -> Result<Option<Arc<RecordReader>>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let record = shapes::record(&record)?;
        let runtime = self.runtime()?;
        let tokio = self.handle()?;
        let reader = self
            .on_node(async move { runtime.pods().read_streamed(identity, pod, record).await })
            .await?;
        Ok(reader.map(|reader| Arc::new(RecordReader::new(reader, tokio))))
    }

    /// A mergeable-document's operations in the order of their ids, each
    /// with its writer.
    pub async fn read_ops(
        &self,
        identity: String,
        pod: String,
        record: PodRecord,
    ) -> Result<Vec<PodOperation>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let record = shapes::record(&record)?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let ops = runtime.pods().read_ops(identity, pod, record).await?;
            Ok(ops.into_iter().map(PodOperation::from).collect())
        })
        .await
    }

    /// Every record an entry of which reads on `identity`'s replica.
    pub async fn list_records(
        &self,
        identity: String,
        pod: String,
    ) -> Result<Vec<PodRecord>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let pod = shapes::pod(&pod)?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let records = runtime.pods().list_records(identity, pod).await?;
            Ok(records.into_iter().map(PodRecord::from).collect())
        })
        .await
    }
}

/// The test build's 2 calls (the `pdn-mobile/host` spec): absent from what
/// ships.
#[cfg(feature = "observation")]
#[uniffi::export]
impl PdnNode {
    /// Serve the test surface on the loopback address at `port` (`0` picks
    /// one), its reset bringing nodes up on `storage_dir`; answers the port
    /// bound. A second call answers the port already bound.
    pub async fn serve_test_surface(
        &self,
        storage_dir: String,
        port: u16,
    ) -> Result<u16, PdnError> {
        let shared = Arc::clone(&self.surface);
        let state = Arc::clone(&self.state);
        self.on_node(
            async move { crate::test_surface::serve(shared, state, storage_dir.into(), port) },
        )
        .await
    }

    /// Keep one device sample for the surface's samples route; refused as
    /// malformed unless it is JSON.
    pub fn report_device_sample(&self, sample: &str) -> Result<(), PdnError> {
        self.surface
            .report(sample)
            .map_err(|err| PdnError::MalformedInput {
                what: format!("a device sample is one JSON value: {err}"),
            })
    }
}

impl PdnNode {
    /// `bring_up` at `connectivity`: a test of this surface runs 2 nodes on
    /// one machine over direct paths, reaching nobody's relay. Not exported.
    pub async fn bring_up_on(
        &self,
        storage_dir: String,
        reconcile_interval_secs: u64,
        connectivity: Connectivity,
    ) -> Result<(), PdnError> {
        let interval = shapes::duration(reconcile_interval_secs, "reconcile interval")?;
        let options = SpawnOptions {
            reconcile_interval: interval,
            connectivity,
            ..SpawnOptions::for_product(storage_dir)
        };
        let (ours, held) = self.claim_bring_up()?;
        let state = Arc::clone(&self.state);
        let installed = self
            .on_node(async move {
                let _held = held;
                install(&state, &ours, Runtime::spawn(options).await).await
            })
            .await?;
        if installed {
            Ok(())
        } else {
            Err(PdnError::NodeNotUp)
        }
    }

    /// The state lock. A poisoned lock means a panic ran while it was held,
    /// which is a defect rather than a refusal.
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, PdnError> {
        self.state.lock().map_err(|_| {
            tracing::error!("the facade's state lock is poisoned");
            PdnError::Internal
        })
    }

    /// Claim the transition into a bring-up, refusing a second one; the
    /// guard goes to the task that installs the node.
    fn claim_bring_up(&self) -> Result<(BringUp, tokio::sync::OwnedMutexGuard<()>), PdnError> {
        let mut state = self.state()?;
        match *state {
            State::Down => {
                let (ours, held) = fresh_bring_up().map_err(|err| {
                    tracing::error!("a bring-up's lock was not free: {err}");
                    PdnError::Internal
                })?;
                *state = State::ComingUp(Arc::clone(&ours));
                Ok((ours, held))
            }
            State::ComingUp(_) | State::Up(_) => Err(PdnError::NodeAlreadyUp),
        }
    }

    /// The facade's asynchronous runtime, for work a writer or a reader
    /// hands it after the call that made them.
    fn handle(&self) -> Result<tokio::runtime::Handle, PdnError> {
        let tokio = self.tokio.lock().map_err(|_| {
            tracing::error!("the facade's runtime lock is poisoned");
            PdnError::Internal
        })?;
        tokio
            .as_ref()
            .map(|tokio| tokio.handle().clone())
            .ok_or(PdnError::Internal)
    }

    /// The node an exported call acts on.
    fn runtime(&self) -> Result<Arc<Runtime>, PdnError> {
        match &*self.state()? {
            State::Up(runtime) => Ok(Arc::clone(runtime)),
            State::Down | State::ComingUp(_) => Err(PdnError::NodeNotUp),
        }
    }

    /// Run one operation on the facade's own asynchronous runtime and map
    /// what it reports through the table. No lock is held across the await.
    async fn on_node<T>(
        &self,
        act: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> Result<T, PdnError>
    where
        T: Send + 'static,
    {
        let joined = {
            let tokio = self.tokio.lock().map_err(|_| {
                tracing::error!("the facade's runtime lock is poisoned");
                PdnError::Internal
            })?;
            match tokio.as_ref() {
                Some(tokio) => tokio.spawn(act),
                // Only reachable while the handle is being released.
                None => return Err(PdnError::Internal),
            }
        };
        match joined.await {
            Ok(outcome) => outcome.map_err(|err| error::table(&err)),
            Err(err) => {
                tracing::error!("an exported call did not finish: {err}");
                Err(PdnError::Internal)
            }
        }
    }
}

/// A bring-up and its task's guard over it.
pub(crate) fn fresh_bring_up() -> anyhow::Result<(BringUp, tokio::sync::OwnedMutexGuard<()>)> {
    let ours: BringUp = Arc::new(tokio::sync::Mutex::new(()));
    let held = Arc::clone(&ours).try_lock_owned()?;
    Ok((ours, held))
}

/// The bring-up's last act, inside the bring-up's own task: put the node
/// where the exported calls look for it, and report whether it went there.
///
/// It does not go there if a stop overtook the bring-up, and then this is
/// what shuts the node down again — otherwise a node nobody holds would
/// keep its endpoint bound and its directory held, and the next bring-up
/// would be refused by a node the caller had already stopped.
pub(crate) async fn install(
    state: &Mutex<State>,
    ours: &BringUp,
    spawned: anyhow::Result<Runtime>,
) -> anyhow::Result<bool> {
    let runtime = match spawned {
        Ok(runtime) => runtime,
        Err(err) => {
            if let Ok(mut guard) = state.lock() {
                if claimed(&guard, ours) {
                    *guard = State::Down;
                }
            }
            return Err(err);
        }
    };
    // Held until the lock says which of the 2 fates is this node's.
    let mut held = Some(runtime);
    let installed = {
        let mut guard = state
            .lock()
            .map_err(|_| anyhow::anyhow!("the facade's state lock is poisoned"))?;
        if claimed(&guard, ours) {
            if let Some(runtime) = held.take() {
                *guard = State::Up(Arc::new(runtime));
            }
            true
        } else {
            false
        }
    };
    if let Some(runtime) = held {
        runtime.shutdown().await?;
    }
    Ok(installed)
}

impl Drop for PdnNode {
    fn drop(&mut self) {
        // A handle released without a stop must not block the thread that
        // let it go, and the node's own teardown does block: the store's
        // handle shuts its actor down and joins its thread as it is
        // dropped. So the runtime and whatever it still holds move to a
        // thread of their own, which stops the node properly and then lets
        // the runtime go.
        let tokio = self.tokio.lock().ok().and_then(|mut held| held.take());
        let node = self
            .state
            .lock()
            .ok()
            .map(|mut guard| std::mem::replace(&mut *guard, State::Down));
        let Some(tokio) = tokio else { return };
        let teardown = std::thread::Builder::new()
            .name("pdn-mobile-teardown".to_owned())
            .spawn(move || {
                match node {
                    Some(State::Up(runtime)) => tokio.block_on(async move {
                        if let Err(err) = runtime.shutdown().await {
                            tracing::error!("a released handle's node did not stop cleanly: {err}");
                        }
                    }),
                    // A transition in flight ends on its own once its task
                    // runs to its lock's release: a stop finishes its
                    // shutdown, and a bring-up finds its claim gone and
                    // shuts its node down (`install`). Letting the runtime
                    // go first would drop that task mid-shutdown.
                    Some(State::ComingUp(theirs)) => tokio.block_on(async move {
                        let _ended = theirs.lock().await;
                    }),
                    Some(State::Down) | None => {}
                }
                drop(tokio);
            });
        if let Err(err) = teardown {
            tracing::error!("a released handle's teardown thread did not start: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tokio::sync::oneshot;

    use super::{claimed, fresh_bring_up, install, Connectivity, PdnError, PdnNode, State};

    /// A stop that overtakes a bring-up returns only once that bring-up's
    /// task is done with its node, so the caller's next bring-up finds the
    /// directory free. The bring-up is staged by hand: the spawn it waits
    /// for has no hook to hold it, and a real one finishes before a stop
    /// arrives unless the network is slow.
    #[tokio::test]
    async fn a_stop_waits_for_the_bring_up_it_overtakes() -> anyhow::Result<()> {
        let node = PdnNode::new()?;
        let (ours, held) = fresh_bring_up()?;
        *node.state()? = State::ComingUp(ours);
        let stopping = {
            let node = Arc::clone(&node);
            tokio::spawn(async move { node.stop().await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !stopping.is_finished(),
            "the stop returned while the bring-up it overtook still held its node"
        );
        drop(held);
        stopping.await??;
        Ok(())
    }

    /// A stop keeps the handle between its states until the node is down:
    /// a bring-up meanwhile is refused as a node already up, and a second
    /// stop meanwhile waits for the first. The node going down is staged as
    /// a bring-up holding its lock, since a real shutdown has no hook to
    /// hold it.
    #[tokio::test]
    async fn a_stop_keeps_the_handle_between_its_states_until_the_node_is_down(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let node = PdnNode::new()?;
        let (theirs, held) = fresh_bring_up()?;
        *node.state()? = State::ComingUp(theirs);
        let first = {
            let node = Arc::clone(&node);
            tokio::spawn(async move { node.stop().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let during = node
            .bring_up_on(
                dir.path().to_string_lossy().into_owned(),
                1,
                Connectivity::Direct,
            )
            .await;
        assert!(
            matches!(during, Err(PdnError::NodeAlreadyUp)),
            "a bring-up during a stop: {during:?}"
        );
        let second = {
            let node = Arc::clone(&node);
            tokio::spawn(async move { node.stop().await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !first.is_finished() && !second.is_finished(),
            "a stop returned while the node it stops was still going down"
        );
        drop(held);
        first.await??;
        second.await??;
        assert!(
            matches!(*node.state()?, State::Down),
            "both stops over, the handle is down"
        );
        Ok(())
    }

    /// A handle released mid-transition lets its runtime go only once the
    /// transition's task is done with its node, rather than dropping the
    /// task mid-shutdown. The task is staged on the facade's runtime: it
    /// holds the transition's lock until the test says, and reports whether
    /// it ran to its end.
    #[tokio::test]
    async fn a_released_handle_waits_for_the_transition_in_flight() -> anyhow::Result<()> {
        let node = PdnNode::new()?;
        let (theirs, held) = fresh_bring_up()?;
        *node.state()? = State::ComingUp(theirs);
        let (release, released) = oneshot::channel::<()>();
        let (done, mut ended) = oneshot::channel::<()>();
        {
            let tokio = node.tokio.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            let tokio = tokio
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no runtime"))?;
            tokio.spawn(async move {
                let _held = held;
                let _ = released.await;
                let _ = done.send(());
            });
        }
        drop(node);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            matches!(ended.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "the released handle's runtime went before the transition in flight ended"
        );
        release
            .send(())
            .map_err(|()| anyhow::anyhow!("the transition's task is gone"))?;
        ended.await?;
        Ok(())
    }

    /// The failed spawn of a bring-up a stop overtook leaves the bring-up
    /// that came after it in place, rather than putting the handle down
    /// under it.
    #[tokio::test]
    async fn an_overtaken_bring_ups_failure_leaves_the_later_one_in_place() -> anyhow::Result<()> {
        let (overtaken, _overtaken_held) = fresh_bring_up()?;
        let (later, _later_held) = fresh_bring_up()?;
        let state = Mutex::new(State::ComingUp(Arc::clone(&later)));
        let spawned = Err(anyhow::anyhow!("another node holds the storage directory"));
        assert!(install(&state, &overtaken, spawned).await.is_err());
        let held = state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        assert!(
            claimed(&held, &later),
            "the overtaken bring-up's failure put the later one's state down"
        );
        Ok(())
    }

    /// Every thread the facade's runtime starts, a worker and a blocking one,
    /// carries the `pdn-node` prefix. The naming sits behind no feature, so
    /// the test build's run of this test speaks for the product build too.
    #[test]
    fn the_runtimes_threads_carry_the_prefix() -> anyhow::Result<()> {
        let node = PdnNode::new()?;
        let held = node.tokio.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        let tokio = held.as_ref().ok_or_else(|| anyhow::anyhow!("no runtime"))?;
        let name = || std::thread::current().name().map(str::to_owned);
        let worker = tokio.block_on(tokio.spawn(async move { name() }))?;
        let blocking = tokio.block_on(tokio.spawn_blocking(name))?;
        for thread in [worker, blocking] {
            assert!(
                thread
                    .as_deref()
                    .is_some_and(|name| name.starts_with("pdn-node")),
                "{thread:?}"
            );
        }
        Ok(())
    }
}
