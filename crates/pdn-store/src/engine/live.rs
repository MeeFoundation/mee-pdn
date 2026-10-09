#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use iroh::{address_lookup::memory::MemoryLookup, Endpoint, EndpointAddr, EndpointId, PublicKey};
use iroh_blobs::{
    api::{
        blobs::BlobStatus,
        downloader::{ContentDiscovery, DownloadRequest, Downloader, SplitStrategy},
        Store,
    },
    Hash, HashAndFormat,
};
use iroh_gossip::net::Gossip;
use n0_future::{
    task::JoinSet,
    time::{Duration, SystemTime},
    FutureExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{self, mpsc, oneshot};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};

// use super::gossip::{GossipActor, ToGossipActor};
use self::draining::DrainingSync;
use super::state::{NamespaceStates, Origin, SyncReason};
use crate::{
    actor::SyncHandle,
    engine::gossip::GossipState,
    metrics::Metrics,
    net::{
        connect_and_sync_asking, handle_in_process_session, handle_session, session_id,
        AbortReason, AcceptError, AcceptOutcome, ConnectError, SessionId, SessionOpening,
        SyncFinished,
    },
    subscribers::{Delivery, LagNotice, Subscribers},
    AuthorHeads, Contact, ContentStatus, Identity, NamespaceId, SignedEntry,
};

mod draining;

/// The stream halves an in-process session runs over: a pipe, since iroh
/// refuses a connection to this endpoint's own id.
pub type InProcessRecv = tokio::io::ReadHalf<tokio::io::DuplexStream>;
/// The writing half of an in-process session's pipe.
pub type InProcessSend = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// Bytes an in-process pipe buffers before the writer waits. One sync
/// message is delivered whole, so the buffer only decides how far ahead
/// the sender runs.
pub(super) const IN_PROCESS_PIPE_BYTES: usize = 64 * 1024;

/// Read the first message of an in-process session; the caller resolved
/// the engine already, so nothing dispatches on it.
pub(super) async fn read_in_process_opening(
    send: InProcessSend,
    recv: InProcessRecv,
    peer: PublicKey,
) -> Result<SessionOpening<InProcessRecv, InProcessSend>, AcceptError> {
    // Bounded like the first message of an accepted connection: the dialing
    // half is a task of this process, and a task that never writes would
    // otherwise hold this one for good.
    n0_future::time::timeout(
        crate::net::SYNC_SESSION_TIMEOUT,
        SessionOpening::read(send, recv, peer),
    )
    .await
    .map_err(|_elapsed| {
        AcceptError::sync(
            peer,
            None,
            anyhow::anyhow!(
                "no in-process init message within {:?}",
                crate::net::SYNC_SESSION_TIMEOUT
            ),
        )
    })?
}

/// An iroh-docs operation
///
/// This is the message that is broadcast over iroh-gossip.
#[derive(Debug, Clone, Serialize, Deserialize, strum::Display)]
pub enum Op {
    /// A new entry was inserted into the document.
    Put(SignedEntry),
    /// A peer now has content available for a hash.
    ContentReady(Hash),
    /// We synced with another peer, here's the news.
    SyncReport(SyncReport),
}

/// Report of a successful sync with the new heads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    namespace: NamespaceId,
    /// Encoded [`AuthorHeads`]
    heads: Vec<u8>,
    /// The identity the sender's replica answers as.
    identity: Identity,
}

/// Messages to the sync actor
#[derive(derive_more::Debug, strum::Display)]
pub enum ToLiveActor {
    StartSync {
        namespace: NamespaceId,
        peers: Vec<Contact>,
        /// The recorded peers dialed beside `peers`: every one the store
        /// holds for the replica when `None`.
        recorded: Option<Vec<PublicKey>>,
        /// Whom a peer no contact or session named is dialed as; `None`
        /// keeps what an earlier start stated, else this engine's identity.
        default_identity: Option<Identity>,
        /// Whether to join the replica's gossip swarm. Scoped access syncs
        /// without ever joining the swarm.
        join_gossip: bool,
        /// An ordered subscriber, registered before the replica is opened
        /// for sync and before the first dial.
        #[debug("observer")]
        observer: Option<async_channel::Sender<Event>>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<anyhow::Result<()>>,
    },
    Leave {
        namespace: NamespaceId,
        kill_subscribers: bool,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<anyhow::Result<()>>,
    },
    LeaveGossip {
        namespace: NamespaceId,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<anyhow::Result<()>>,
    },
    Shutdown {
        reply: sync::oneshot::Sender<()>,
    },
    Subscribe {
        namespace: NamespaceId,
        #[debug("sender")]
        sender: async_channel::Sender<Event>,
        #[debug("oneshot::Sender")]
        reply: sync::oneshot::Sender<Result<()>>,
    },
    /// An ordered subscriber; refused for a namespace not syncing, whose
    /// inserts this actor never receives.
    Observe {
        namespace: NamespaceId,
        #[debug("sender")]
        sender: async_channel::Sender<Event>,
        #[debug("oneshot::Sender")]
        reply: sync::oneshot::Sender<Result<()>>,
    },
    /// A session dispatched here by the identity its first message named.
    HandleSession {
        conn: iroh::endpoint::Connection,
        #[debug("SessionOpening")]
        opening: SessionOpening<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    },
    /// The serving half of a session between two identities of this node.
    AcceptInProcess {
        #[debug("SessionOpening")]
        opening: SessionOpening<InProcessRecv, InProcessSend>,
    },
    /// The content `namespace`'s entries lack, as the scan its first
    /// `start_sync` started read it off the loop.
    ContentScanned {
        namespace: NamespaceId,
        missing: Vec<Hash>,
    },
    /// A dial the remote aborted as already syncing, asked for again.
    Redial {
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
    },
    /// The exchanges of `namespaces` running, held behind another's, or
    /// waiting to redial.
    SyncsInFlight {
        namespaces: Vec<NamespaceId>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<usize>,
    },
    /// Whom each peer of `namespace` is dialed as, replacing what contacts
    /// stated before; dials nothing.
    StateContacts {
        namespace: NamespaceId,
        contacts: Vec<Contact>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<()>,
    },
    /// Every later dial of `namespace` follows an exchange of `first` with
    /// the same counterpart; `None` lifts it.
    OrderAfter {
        namespace: NamespaceId,
        first: Option<NamespaceId>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<()>,
    },
    SyncInProcess {
        namespace: NamespaceId,
        callee: Identity,
        peer: PublicKey,
        #[debug("pipe")]
        send: InProcessSend,
        #[debug("pipe")]
        recv: InProcessRecv,
    },
    AcceptSyncRequest {
        namespace: NamespaceId,
        peer: PublicKey,
        /// The identity the caller acts for: two identities of one node are
        /// two callers at one node id (ADR-0013).
        caller: Identity,
        #[debug("oneshot::Sender")]
        reply: sync::oneshot::Sender<AcceptOutcome>,
    },

    IncomingSyncReport {
        from: PublicKey,
        report: SyncReport,
    },
    NeighborContentReady {
        namespace: NamespaceId,
        node: PublicKey,
        hash: Hash,
    },
    NeighborUp {
        namespace: NamespaceId,
        peer: PublicKey,
    },
    NeighborDown {
        namespace: NamespaceId,
        peer: PublicKey,
    },
}

/// Events informing about actions of the live sync progress.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq, strum::Display)]
pub enum Event {
    /// A local insertion; only ordered subscribers receive it.
    LocalInsert { entry: SignedEntry },
    /// A remote insertion; only ordered subscribers receive it.
    RemoteInsert {
        from: crate::PeerIdBytes,
        entry: SignedEntry,
    },
    /// The content of an entry was downloaded and is now available at the local node
    ContentReady {
        /// The content hash of the newly available entry content
        hash: Hash,
    },
    /// We have a new neighbor in the swarm.
    NeighborUp(PublicKey),
    /// We lost a neighbor in the swarm.
    NeighborDown(PublicKey),
    /// A set-reconciliation sync finished.
    SyncFinished(SyncEvent),
    /// All pending content is now ready.
    ///
    /// This event is only emitted after a sync completed and `Self::SyncFinished` was emitted at
    /// least once. It signals that all currently pending downloads have been completed.
    ///
    /// Receiving this event does not guarantee that all content in the document is available. If
    /// blobs failed to download, this event will still be emitted after all operations completed.
    PendingContentReady,
    /// Events were dropped since the last one received: the subscription's
    /// buffer was full.
    Lagged,
}

impl LagNotice for Event {
    fn lagged(&self) -> Self {
        Self::Lagged
    }
}

/// The identity is the callee the dial addressed: a node of two identities
/// is two counterparts at one node id (ADR-0013).
type SyncConnectRes = (
    NamespaceId,
    PublicKey,
    Identity,
    SyncReason,
    Option<SessionId>,
    Result<SyncFinished, ConnectError>,
);
/// The namespace and peer the session was opened on, then the identity it
/// named as its caller: the pair is reported even when the exchange fails,
/// because the identity it put in `peer_identities` is released by it.
type SyncAcceptRes = (
    NamespaceId,
    PublicKey,
    Identity,
    Option<SessionId>,
    Result<SyncFinished, AcceptError>,
);
type DownloadRes = (NamespaceId, Hash, Result<(), anyhow::Error>);

/// The identities one peer of one replica is dialed as. Two sources, held
/// apart because they are trusted and live differently: a contact is the
/// consumer's own statement and stays, while a session's first message is
/// the caller's word and stays only as long as that admitted session —
/// otherwise any reachable peer could name identities until the map exhausts
/// memory.
#[derive(Default, Debug)]
struct PeerIdentities {
    stated: BTreeSet<Identity>,
    /// Counted, because two sessions of one identity can run at once.
    /// Taken when the access provider admits the session, given back when
    /// it ends, so a refused caller leaves nothing.
    in_session: BTreeMap<Identity, usize>,
}

impl PeerIdentities {
    fn all(&self) -> impl Iterator<Item = Identity> + '_ {
        self.stated
            .iter()
            .chain(self.in_session.keys())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
    }

    fn is_empty(&self) -> bool {
        self.stated.is_empty() && self.in_session.is_empty()
    }

    fn enter(&mut self, identity: Identity) {
        *self.in_session.entry(identity).or_default() += 1;
    }

    fn leave(&mut self, identity: Identity) {
        if let std::collections::btree_map::Entry::Occupied(mut entry) =
            self.in_session.entry(identity)
        {
            *entry.get_mut() -= 1;
            if *entry.get() == 0 {
                entry.remove();
            }
        }
    }
}

/// How long after a remote aborted a dial as already syncing it is asked
/// for again: enough for the remote's own exchange to have run. Each
/// further abort in a row doubles it.
const REDIAL_AFTER_ABORT: Duration = Duration::from_millis(500);

/// Redials of one pair in a row before an abort is left to the next
/// trigger: the remote picks the abort's reason, so without a bound a
/// remote answering every dial as already syncing keeps one going forever.
const MAX_REDIALS: u32 = 3;

/// A dial held until the exchange it follows finishes.
enum HeldDial {
    Network {
        namespace: NamespaceId,
        reason: SyncReason,
    },
    InProcess {
        namespace: NamespaceId,
        send: InProcessSend,
        recv: InProcessRecv,
    },
}

impl HeldDial {
    fn namespace(&self) -> NamespaceId {
        match self {
            Self::Network { namespace, .. } | Self::InProcess { namespace, .. } => *namespace,
        }
    }
}

// Currently peers might double-sync in both directions.
pub struct LiveActor {
    /// Receiver for actor messages.
    inbox: mpsc::Receiver<ToLiveActor>,
    sync: DrainingSync,
    endpoint: Endpoint,
    bao_store: Store,
    held_payloads: Arc<super::held::HeldPayloads>,
    downloader: Downloader,
    memory_lookup: MemoryLookup,

    /// Send messages to self.
    /// Note: Must not be used in methods called from `Self::run` directly to prevent deadlocks.
    /// Only clone into newly spawned tasks.
    sync_actor_tx: mpsc::Sender<ToLiveActor>,
    gossip: GossipState,

    /// Running sync futures (from connect).
    running_sync_connect: JoinSet<SyncConnectRes>,
    /// Running sync futures (from accept).
    running_sync_accept: JoinSet<SyncAcceptRes>,
    /// Running download futures.
    download_tasks: JoinSet<DownloadRes>,
    /// Content hashes which are wanted but not yet queued because no provider was found,
    /// keyed by the namespace whose entry wants them (the namespace drives retries on
    /// sync-finished and keeps `PendingContentReady` attribution correct).
    missing_hashes: HashSet<(NamespaceId, Hash)>,
    /// Per namespace whose content scan runs, the peer of the last session
    /// that went through meanwhile: its retry of parked content ran before
    /// the scan parked anything.
    content_scans: HashMap<NamespaceId, Option<PublicKey>>,
    /// Queued content whose running download should be retried once if it fails: a fresh
    /// provider was registered (through a finished sync) after the running download had
    /// already snapshotted its provider set, so the provider is only reachable by a new
    /// download attempt.
    retry_after_failure: HashSet<(NamespaceId, Hash)>,
    /// Content hashes queued in downloader.
    queued_hashes: QueuedHashes,
    /// Nodes known to have a hash
    hash_providers: ProviderNodes,

    /// Subscribers to actor events
    subscribers: SubscribersMap,

    /// Sync state per replica and peer
    state: NamespaceStates,
    /// The embedder's per-session access provider: consulted on both
    /// session roles — accept and dial — to decide what a peer may see of
    /// a namespace.
    session_access: crate::filter::SessionAccessProvider,
    /// The identity every replica of this engine is held for, and the one
    /// its dials act as.
    identity: Identity,
    /// Which identities a peer is dialed as, per replica. One node id hosts
    /// several (ADR-0013), so the value is a set: a single slot would let
    /// each of them displace the others and leave all but the last
    /// undialed.
    peer_identities: HashMap<(NamespaceId, PublicKey), PeerIdentities>,
    /// Whom a peer of a replica is dialed as when nothing named one — the
    /// consumer's statement, since only it knows whose replica this is.
    default_identities: HashMap<NamespaceId, Identity>,
    /// The namespace whose exchange with a counterpart every dial of a
    /// namespace follows.
    prerequisites: HashMap<NamespaceId, NamespaceId>,
    /// Dials held until the prerequisite's exchange with the same
    /// counterpart finishes, keyed by that exchange.
    held_dials: HashMap<(NamespaceId, PublicKey, Identity), Vec<HeldDial>>,
    /// Redials waiting out [`REDIAL_AFTER_ABORT`], by namespace.
    redials_due: HashMap<NamespaceId, usize>,
    /// Aborts as already syncing in a row per pair, cleared by an exchange
    /// that finishes.
    redial_streaks: HashMap<(NamespaceId, PublicKey, Identity), u32>,
    /// In-process sessions opened, so a pass over a quiet pair can be
    /// shown to open none.
    in_process_sessions: Arc<AtomicU64>,
    /// Where a local write and a contact naming this node are handed to
    /// the node, which alone knows its other identities; `None` hands them
    /// to nobody.
    co_located: Option<crate::engine::CoLocatedRequests>,
    metrics: Arc<Metrics>,
}
impl LiveActor {
    /// Create the live actor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sync: SyncHandle,
        endpoint: Endpoint,
        gossip: Gossip,
        bao_store: Store,
        held_payloads: Arc<super::held::HeldPayloads>,
        downloader: Downloader,
        inbox: mpsc::Receiver<ToLiveActor>,
        sync_actor_tx: mpsc::Sender<ToLiveActor>,
        session_access: crate::filter::SessionAccessProvider,
        identity: Identity,
        in_process_sessions: Arc<AtomicU64>,
        co_located: Option<crate::engine::CoLocatedRequests>,
        metrics: Arc<Metrics>,
    ) -> Result<Self> {
        let gossip_state = GossipState::new(gossip, sync_actor_tx.clone());
        let memory_lookup = MemoryLookup::new();
        endpoint.address_lookup()?.add(memory_lookup.clone());
        Ok(Self {
            inbox,
            sync: DrainingSync::new(sync),
            endpoint,
            memory_lookup,
            gossip: gossip_state,
            bao_store,
            held_payloads,
            downloader,
            sync_actor_tx,
            running_sync_connect: Default::default(),
            running_sync_accept: Default::default(),
            subscribers: Default::default(),
            download_tasks: Default::default(),
            state: Default::default(),
            missing_hashes: Default::default(),
            content_scans: Default::default(),
            retry_after_failure: Default::default(),
            queued_hashes: Default::default(),
            hash_providers: Default::default(),
            session_access,
            identity,
            peer_identities: Default::default(),
            default_identities: Default::default(),
            prerequisites: Default::default(),
            held_dials: Default::default(),
            redials_due: Default::default(),
            redial_streaks: Default::default(),
            in_process_sessions,
            co_located,
            metrics,
        })
    }

    /// Best effort: the node's periodic pass over its co-located pairs
    /// reconciles whatever a request lost to a full channel would have.
    fn ask_co_located(&self, request: crate::engine::CoLocatedRequest) {
        if let Some(requests) = &self.co_located {
            let _ = requests.try_send(request);
        }
    }

    /// Run the actor loop.
    pub async fn run(mut self) -> Result<()> {
        let shutdown_reply = self.run_inner().await;
        if let Err(err) = self.shutdown().await {
            error!(?err, "Error during shutdown");
        }
        drop(self);
        match shutdown_reply {
            Ok(reply) => {
                reply.send(()).ok();
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    async fn run_inner(&mut self) -> Result<oneshot::Sender<()>> {
        let mut i = 0;
        loop {
            i += 1;
            trace!(?i, "tick wait");
            self.metrics.doc_live_tick_main.inc();
            tokio::select! {
                biased;
                msg = self.inbox.recv() => {
                    let msg = msg.context("to_actor closed")?;
                    trace!(?i, %msg, "tick: to_actor");
                    self.metrics.doc_live_tick_actor.inc();
                    match msg {
                        ToLiveActor::Shutdown { reply } => {
                            break Ok(reply);
                        }
                        msg => {
                            self.on_actor_message(msg).await.context("on_actor_message")?;
                        }
                    }
                }
                event = self.sync.next_event() => {
                    trace!(?i, "tick: replica_event");
                    self.metrics.doc_live_tick_replica_event.inc();
                    let event = event.context("replica_events closed")?;
                    if let Err(err) = self.on_replica_event(event).await {
                        error!(?err, "Failed to process replica event");
                    }
                }
                Some(res) = self.running_sync_connect.join_next(), if !self.running_sync_connect.is_empty() => {
                    trace!(?i, "tick: running_sync_connect");
                    self.metrics.doc_live_tick_running_sync_connect.inc();
                    let (namespace, peer, callee, reason, session, res) = res.context("running_sync_connect closed")?;
                    self.on_sync_via_connect_finished(namespace, peer, callee, reason, session, res).await;

                }
                Some(res) = self.running_sync_accept.join_next(), if !self.running_sync_accept.is_empty() => {
                    trace!(?i, "tick: running_sync_accept");
                    self.metrics.doc_live_tick_running_sync_accept.inc();
                    let (namespace, peer, caller, session, res) = res.context("running_sync_accept closed")?;
                    self.release_peer_identity(namespace, peer, caller);
                    self.on_sync_via_accept_finished(caller, session, res).await;
                }
                Some(res) = self.download_tasks.join_next(), if !self.download_tasks.is_empty() => {
                    trace!(?i, "tick: pending_downloads");
                    self.metrics.doc_live_tick_pending_downloads.inc();
                    let (namespace, hash, res) = res.context("pending_downloads closed")?;
                    self.on_download_ready(namespace, hash, res).await;
                }
                res = self.gossip.progress(), if !self.gossip.is_empty() => {
                    if let Err(error) = res {
                        warn!(?error, "gossip state failed");
                    }
                }
            }
        }
    }

    async fn on_actor_message(&mut self, msg: ToLiveActor) -> anyhow::Result<bool> {
        match msg {
            ToLiveActor::Shutdown { .. } => {
                unreachable!("handled in run");
            }
            ToLiveActor::IncomingSyncReport { from, report } => {
                self.on_sync_report(from, report).await
            }
            ToLiveActor::NeighborUp { namespace, peer } => {
                debug!(peer = %peer.fmt_short(), namespace = %namespace.fmt_short(), "neighbor up");
                self.sync_with_peer(namespace, peer, SyncReason::NewNeighbor);
                self.subscribers
                    .send(&namespace, Event::NeighborUp(peer))
                    .await;
            }
            ToLiveActor::NeighborDown { namespace, peer } => {
                debug!(peer = %peer.fmt_short(), namespace = %namespace.fmt_short(), "neighbor down");
                self.subscribers
                    .send(&namespace, Event::NeighborDown(peer))
                    .await;
            }
            ToLiveActor::StartSync {
                namespace,
                peers,
                recorded,
                default_identity,
                join_gossip,
                observer,
                reply,
            } => {
                if let Some(observer) = &observer {
                    self.subscribers.observe(namespace, observer.clone());
                }
                let res = self
                    .start_sync(namespace, peers, recorded, default_identity, join_gossip)
                    .await;
                if let (Err(_), Some(observer)) = (&res, &observer) {
                    self.subscribers.unobserve(namespace, observer);
                }
                reply.send(res).ok();
            }
            ToLiveActor::Leave {
                namespace,
                kill_subscribers,
                reply,
            } => {
                let res = self.leave(namespace, kill_subscribers).await;
                reply.send(res).ok();
            }
            ToLiveActor::LeaveGossip { namespace, reply } => {
                self.leave_gossip(namespace);
                reply.send(Ok(())).ok();
            }
            ToLiveActor::Subscribe {
                namespace,
                sender,
                reply,
            } => {
                self.subscribers.subscribe(namespace, sender);
                reply.send(Ok(())).ok();
            }
            ToLiveActor::Observe {
                namespace,
                sender,
                reply,
            } => {
                let res = if self.state.is_syncing(&namespace) {
                    self.subscribers.observe(namespace, sender);
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("{} is not syncing", namespace.fmt_short()))
                };
                reply.send(res).ok();
            }
            ToLiveActor::HandleSession { conn, opening } => {
                self.handle_session(conn, opening).await;
            }
            ToLiveActor::AcceptInProcess { opening } => {
                self.accept_in_process(opening);
            }
            ToLiveActor::ContentScanned { namespace, missing } => {
                self.on_content_scanned(namespace, missing).await;
            }
            ToLiveActor::Redial {
                namespace,
                peer,
                callee,
                reason,
            } => {
                if let Some(due) = self.redials_due.get_mut(&namespace) {
                    *due = due.saturating_sub(1);
                }
                self.sync_with_identity(namespace, peer, callee, reason);
            }
            ToLiveActor::SyncsInFlight { namespaces, reply } => {
                reply.send(self.syncs_in_flight(&namespaces)).ok();
            }
            ToLiveActor::StateContacts {
                namespace,
                contacts,
                reply,
            } => {
                self.state_contacts(namespace, contacts);
                reply.send(()).ok();
            }
            ToLiveActor::OrderAfter {
                namespace,
                first,
                reply,
            } => {
                match first {
                    Some(first) => self.prerequisites.insert(namespace, first),
                    None => self.prerequisites.remove(&namespace),
                };
                reply.send(()).ok();
            }
            ToLiveActor::SyncInProcess {
                namespace,
                callee,
                peer,
                send,
                recv,
            } => {
                self.sync_in_process(namespace, callee, peer, send, recv);
            }
            ToLiveActor::AcceptSyncRequest {
                namespace,
                peer,
                caller,
                reply,
            } => {
                // Reaching here means the access provider admitted the
                // caller, so this is where its identity is learned.
                self.peer_identities
                    .entry((namespace, peer))
                    .or_default()
                    .enter(caller);
                let outcome = self.accept_sync_request(namespace, peer, caller);
                reply.send(outcome).ok();
            }
            ToLiveActor::NeighborContentReady {
                namespace,
                node,
                hash,
            } => {
                self.on_neighbor_content_ready(namespace, node, hash).await;
            }
        };
        Ok(true)
    }

    /// Which identity `peer` is dialed as for `namespace`: what a contact
    /// or a past session named, else the consumer's default for the
    /// replica, else this engine's own identity — what a sibling device of
    /// the same identity is.
    /// Give back the identity a finished session named, and drop the pair
    /// once nothing names it: a session that was refused leaves no trace,
    /// and one that ran leaves none either.
    fn release_peer_identity(&mut self, namespace: NamespaceId, peer: PublicKey, caller: Identity) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.peer_identities.entry((namespace, peer))
        {
            entry.get_mut().leave(caller);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// Every identity this peer is dialed as for `namespace`: what contacts
    /// and running sessions named, else the consumer's default for the
    /// replica, else this engine's own identity — what a sibling device of
    /// the same identity is.
    fn identities_of_peer(&self, namespace: NamespaceId, peer: PublicKey) -> Vec<Identity> {
        let learned = self.learned_identities(namespace, peer);
        if !learned.is_empty() {
            return learned;
        }
        match self.default_identities.get(&namespace) {
            Some(default) => vec![*default],
            None => vec![self.identity],
        }
    }

    /// The identities contacts and running sessions name this peer as.
    fn learned_identities(&self, namespace: NamespaceId, peer: PublicKey) -> Vec<Identity> {
        self.peer_identities
            .get(&(namespace, peer))
            .map(|identities| identities.all().collect())
            .unwrap_or_default()
    }

    /// A pull of the news `peer` reported, addressed as contacts and running
    /// sessions name it, else as the identity the report names when the
    /// consumer named that identity for the replica, else as
    /// [`identities_of_peer`](Self::identities_of_peer) does. The report's
    /// name is the sender's word, so it picks among the consumer's names
    /// alone: a sender naming fresh identities would otherwise open a slot
    /// and a dial for each.
    fn sync_with_reporter(&mut self, namespace: NamespaceId, peer: PublicKey, reporter: Identity) {
        let learned = self.learned_identities(namespace, peer);
        let callees = if !learned.is_empty() {
            learned
        } else if self.consumer_names(namespace, reporter) {
            vec![reporter]
        } else {
            self.identities_of_peer(namespace, peer)
        };
        for callee in callees {
            self.sync_with_identity(namespace, peer, callee, SyncReason::SyncReport);
        }
    }

    /// Whether the consumer named `identity` for `namespace`: as a contact's
    /// or as the replica's default.
    fn consumer_names(&self, namespace: NamespaceId, identity: Identity) -> bool {
        self.default_identities.get(&namespace) == Some(&identity)
            || self
                .peer_identities
                .iter()
                .any(|((tracked, _peer), identities)| {
                    *tracked == namespace && identities.stated.contains(&identity)
                })
    }

    /// One dial per identity the peer is known as: co-located identities of one
    /// node id each get their own exchange, since one session addresses one.
    fn sync_with_peer(&mut self, namespace: NamespaceId, peer: PublicKey, reason: SyncReason) {
        for callee in self.identities_of_peer(namespace, peer) {
            self.sync_with_identity(namespace, peer, callee, reason);
        }
    }

    #[instrument("connect", skip_all, fields(peer = %peer.fmt_short(), namespace = %namespace.fmt_short()))]
    fn sync_with_identity(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
    ) {
        // iroh refuses a connection to this endpoint's own id before it
        // looks at an address, so a contact that names this node — from a
        // ticket, a device record, a contact list — reaches its identity
        // inside the process or not at all.
        if peer == self.endpoint.id() {
            self.ask_co_located(crate::engine::CoLocatedRequest::Dial {
                namespace,
                caller: self.identity,
                callee,
            });
            return;
        }
        if let Some(first) = self.prerequisites.get(&namespace).copied() {
            if !self.state.is_running(&first, (peer, callee)) {
                self.dial(first, peer, callee, reason);
            }
            if self.state.is_running(&first, (peer, callee)) {
                // One held dial per trigger: merged, the second no longer
                // queues its resync at release, and
                // `two_nodes_sharing_sixty_pods_keep_live_delivery_in_each`
                // in data-layer misses a claim in about one run in ten.
                self.held_dials
                    .entry((first, peer, callee))
                    .or_default()
                    .push(HeldDial::Network { namespace, reason });
                return;
            }
        }
        self.dial(namespace, peer, callee, reason);
    }

    fn dial(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
    ) {
        if !self.state.start_connect(&namespace, peer, callee, reason) {
            return;
        }
        let endpoint = self.endpoint.clone();
        let sync = self.sync.handle().clone();
        let metrics = self.metrics.clone();
        let session_access = self.session_access.clone();
        let caller = self.identity;
        let fut = async move {
            // The dialing side serves entries too (reconciliation is
            // bidirectional), so the embedder's access provider gates this
            // role exactly like the accept role, asked once the session's
            // snapshot is open.
            let mut session = None;
            let res = connect_and_sync_asking(
                &endpoint,
                &sync,
                namespace,
                callee,
                caller,
                EndpointAddr::new(peer),
                Some(&metrics),
                &session_access,
                &mut session,
            )
            .await;
            (namespace, peer, callee, reason, session, res)
        }
        .instrument(Span::current());
        self.running_sync_connect.spawn(fut);
    }

    /// The dialing half of a session between two identities of this node.
    /// The pipe replaces the transport and nothing else: the same codec,
    /// the same session setup, the same access provider call.
    fn sync_in_process(
        &mut self,
        namespace: NamespaceId,
        callee: Identity,
        peer: PublicKey,
        mut send: InProcessSend,
        mut recv: InProcessRecv,
    ) {
        // The consumer opens the prerequisite's in-process exchange first;
        // one still running holds this dial until it finishes.
        if let Some(first) = self.prerequisites.get(&namespace).copied() {
            if self.state.is_running(&first, (peer, callee)) {
                self.held_dials
                    .entry((first, peer, callee))
                    .or_default()
                    .push(HeldDial::InProcess {
                        namespace,
                        send,
                        recv,
                    });
                return;
            }
        }
        // Announced, so a pair found busy queues a resync. The replay dials
        // through `sync_with_peer`, which on this identity's own namespace
        // resolves to this identity itself and reaches no co-located one.
        let reason = SyncReason::Announced;
        if !self.state.start_connect(&namespace, peer, callee, reason) {
            return;
        }
        let sync = self.sync.handle().clone();
        let metrics = self.metrics.clone();
        let session_access = self.session_access.clone();
        let caller = self.identity;
        self.in_process_sessions.fetch_add(1, Ordering::Relaxed);
        let fut = async move {
            let res = crate::net::sync_in_process(
                &sync,
                namespace,
                callee,
                caller,
                peer,
                &mut send,
                &mut recv,
                Some(&metrics),
                &session_access,
            )
            .await;
            (namespace, peer, callee, reason, None, res)
        }
        .instrument(Span::current());
        self.running_sync_connect.spawn(fut);
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        // cancel all subscriptions
        self.subscribers.clear();
        let (gossip_shutdown_res, _store) = tokio::join!(
            // quit the gossip topics and task loops.
            self.gossip.shutdown(),
            // shutdown sync thread
            self.sync.shutdown()
        );
        gossip_shutdown_res?;
        // TODO: abort_all and join_next all JoinSets to catch panics
        // (they are aborted on drop, but that swallows panics)
        Ok(())
    }

    async fn start_sync(
        &mut self,
        namespace: NamespaceId,
        peers: Vec<Contact>,
        handed: Option<Vec<PublicKey>>,
        default_identity: Option<Identity>,
        join_gossip: bool,
    ) -> Result<()> {
        // A peer the engine recorded carries a node id and nothing else,
        // so the consumer states whom a peer of this replica is dialed as
        // when neither a contact nor a past session named one.
        match default_identity {
            Some(identity) => {
                self.default_identities.insert(namespace, identity);
            }
            None => {
                self.default_identities
                    .entry(namespace)
                    .or_insert(self.identity);
            }
        }
        let mut recorded: Vec<PublicKey> = Vec::new();
        debug!(?namespace, peers = peers.len(), join_gossip, "start sync");
        // update state to allow sync
        if !self.state.is_syncing(&namespace) {
            self.sync.open(namespace).await?;
            self.state.insert(namespace);
            self.scan_missing_content(namespace);
        }
        if let Some(handed) = handed {
            self.join_peers(namespace, peers, handed, join_gossip)
                .await?;
            return Ok(());
        }
        // add the peers stored for this document
        match self.sync.get_sync_peers(namespace).await {
            Ok(None) => {
                // no peers for this document
            }
            Ok(Some(known_useful_peers)) => {
                // A peer the engine recorded carries a node id and no
                // identity, so it is dialed as whatever this replica already
                // learned for it — never recorded with an identity of its own,
                // or a guess would stick and outlive what a contact says.
                recorded.extend(known_useful_peers.into_iter().filter_map(|peer_id_bytes| {
                    // peers are stored as bytes, don't fail the operation if they can't be
                    // decoded: simply ignore the peer
                    match PublicKey::from_bytes(&peer_id_bytes) {
                        Ok(public_key) => Some(public_key),
                        Err(_signing_error) => {
                            warn!("potential db corruption: peers per doc can't be decoded");
                            None
                        }
                    }
                }));
            }
            Err(e) => {
                // try to continue if peers per doc can't be read since they are not vital for sync
                warn!(%e, "db error reading peers per document")
            }
        }
        self.join_peers(namespace, peers, recorded, join_gossip)
            .await?;
        Ok(())
    }

    /// Find, off the loop, the content of every entry of `namespace` the
    /// download policy wants and the blob store does not hold whole, and
    /// park it as a remote insert parks it ([`ToLiveActor::ContentScanned`]).
    /// The parked set lives in memory: without this, content whose download
    /// a restart cut short is never asked for again, since a session after
    /// the restart finds no entry to exchange. Off the loop, since the scan
    /// reads every entry and asks the blob store about each.
    fn scan_missing_content(&mut self, namespace: NamespaceId) {
        self.content_scans.insert(namespace, None);
        let sync = self.sync.handle().clone();
        let blobs = self.bao_store.clone();
        let to_self = self.sync_actor_tx.clone();
        n0_future::task::spawn(async move {
            let missing = missing_content(&sync, &blobs, namespace)
                .await
                .unwrap_or_else(|err| {
                    warn!(
                        ?namespace,
                        "finding the content its entries lack failed: {err:#}"
                    );
                    Vec::new()
                });
            to_self
                .send(ToLiveActor::ContentScanned { namespace, missing })
                .await
                .ok();
        });
    }

    async fn on_content_scanned(&mut self, namespace: NamespaceId, missing: Vec<Hash>) {
        let met = self.content_scans.remove(&namespace).flatten();
        if !self.state.is_syncing(&namespace) {
            return;
        }
        for hash in missing {
            self.missing_hashes.insert((namespace, hash));
            if let Some(peer) = met {
                self.start_download(namespace, hash, peer, true).await;
            }
        }
    }

    async fn leave(
        &mut self,
        namespace: NamespaceId,
        kill_subscribers: bool,
    ) -> anyhow::Result<()> {
        // self.subscribers.remove(&namespace);
        self.prerequisites.remove(&namespace);
        let held_on_it: Vec<(NamespaceId, PublicKey, Identity)> = self
            .held_dials
            .keys()
            .filter(|(first, _peer, _counterpart)| *first == namespace)
            .copied()
            .collect();
        for dials in self.held_dials.values_mut() {
            dials.retain(|held| held.namespace() != namespace);
        }
        if self.state.remove(&namespace) {
            self.peer_identities
                .retain(|(tracked, _peer), _identities| *tracked != namespace);
            self.redial_streaks
                .retain(|(tracked, _peer, _callee), _streak| *tracked != namespace);
            self.sync.set_sync(namespace, false).await?;
            self.sync.unsubscribe(namespace).await?;
            self.sync.close(namespace).await?;
            self.gossip.quit(&namespace);
        }
        if kill_subscribers {
            self.subscribers.remove(&namespace);
        }
        for (first, peer, counterpart) in held_on_it {
            self.release_held(first, peer, counterpart);
        }
        Ok(())
    }

    /// The stated identities of `namespace`'s peers become `contacts`'
    /// alone; the ones a running session named stay until it ends.
    fn syncs_in_flight(&self, namespaces: &[NamespaceId]) -> usize {
        let running: usize = namespaces
            .iter()
            .map(|namespace| self.state.running(namespace))
            .sum();
        let held =
            self.held_dials
                .iter()
                .flat_map(|((first, _peer, _counterpart), dials)| {
                    dials.iter().map(move |dial| (first, dial))
                })
                .filter(|(first, dial)| {
                    let namespace = match dial {
                        HeldDial::Network { namespace, .. }
                        | HeldDial::InProcess { namespace, .. } => namespace,
                    };
                    namespaces.contains(first) || namespaces.contains(namespace)
                })
                .count();
        let due: usize = namespaces
            .iter()
            .filter_map(|namespace| self.redials_due.get(namespace))
            .sum();
        running.saturating_add(held).saturating_add(due)
    }

    fn state_contacts(&mut self, namespace: NamespaceId, contacts: Vec<Contact>) {
        for ((tracked, _peer), identities) in &mut self.peer_identities {
            if *tracked == namespace {
                identities.stated.clear();
            }
        }
        for Contact { addr, identity } in contacts {
            self.peer_identities
                .entry((namespace, addr.id))
                .or_default()
                .stated
                .insert(identity);
            if !addr.is_empty() {
                self.memory_lookup.add_endpoint_info(addr);
            }
        }
        self.peer_identities
            .retain(|_key, identities| !identities.is_empty());
    }

    /// Run the dials held on the exchange of `first` with this
    /// counterpart, which just ended.
    fn release_held(&mut self, first: NamespaceId, peer: PublicKey, counterpart: Identity) {
        for held in self
            .held_dials
            .remove(&(first, peer, counterpart))
            .unwrap_or_default()
        {
            match held {
                HeldDial::Network { namespace, reason } => {
                    self.dial(namespace, peer, counterpart, reason);
                }
                HeldDial::InProcess {
                    namespace,
                    send,
                    recv,
                } => self.sync_in_process(namespace, counterpart, peer, send, recv),
            }
        }
    }

    /// Leave the replica's gossip swarm and touch nothing else — the
    /// narrow inverse of the join `start_sync` performs.
    ///
    /// The replica stays open and in the sync set — reconciliation keeps
    /// accepting and dialing — and event subscribers stay live. Quitting
    /// drops both halves of the topic subscription (the receive loop is
    /// aborted, the sender goes with its state), so gossip stops in both
    /// directions: nothing more is ingested from the topic, and
    /// broadcasting to it becomes a no-op. A later gossip-joining
    /// `start_sync` re-subscribes. Idempotent: quitting a topic that was
    /// never joined does nothing.
    fn leave_gossip(&mut self, namespace: NamespaceId) {
        self.gossip.quit(&namespace);
    }

    async fn join_peers(
        &mut self,
        namespace: NamespaceId,
        peers: Vec<Contact>,
        recorded: Vec<PublicKey>,
        join_gossip: bool,
    ) -> Result<()> {
        let mut peer_ids = Vec::new();

        // add addresses of peers to our endpoint address book
        for Contact { addr, identity } in peers.into_iter() {
            let peer_id = addr.id;
            // The contact names the identity it is dialed as, so a later
            // dial reaches the same one however it was triggered.
            self.peer_identities
                .entry((namespace, peer_id))
                .or_default()
                .stated
                .insert(identity);
            // adding a node address without any addressing info fails with an error,
            // but we still want to include those peers because endpoint address lookup might find addresses for them
            if !addr.is_empty() {
                self.memory_lookup.add_endpoint_info(addr);
            }
            peer_ids.push(peer_id);
        }
        for peer_id in recorded {
            if !peer_ids.contains(&peer_id) {
                peer_ids.push(peer_id);
            }
        }

        // tell gossip to join — unless this is scoped access, which stays
        // outside the replica's swarm: reconciliation is its only data
        // path, and the direct syncs below still run.
        if join_gossip {
            self.gossip.join(namespace, peer_ids.clone()).await?;
        }

        if !peer_ids.is_empty() {
            // trigger initial sync with initial peers
            for peer in peer_ids {
                self.sync_with_peer(namespace, peer, SyncReason::DirectJoin);
            }
        }
        Ok(())
    }

    #[instrument("connect", skip_all, fields(peer = %peer.fmt_short(), namespace = %namespace.fmt_short()))]
    async fn on_sync_via_connect_finished(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
        session: Option<SessionId>,
        result: Result<SyncFinished, ConnectError>,
    ) {
        match result {
            Err(ConnectError::RemoteAbort(AbortReason::AlreadySyncing)) => {
                debug!(?reason, "remote abort, already syncing");
                // The remote refused our dial because it sees an exchange with us already
                // running. Nothing else will finish the dial recorded in our state, so clear
                // it — otherwise this (namespace, peer) pair stays `Running` forever and every
                // later sync trigger for it is silently dropped.
                self.state.abort_connect(&namespace, peer, callee, reason);
                // An incoming exchange that took the slot over releases the
                // held dials when it finishes; with none, nothing else would.
                if !self.state.is_running(&namespace, (peer, callee)) {
                    self.release_held(namespace, peer, callee);
                }
                // The remote's exchange may be one this side refuses — a
                // replica holding nothing yet judges no caller — and then
                // neither runs; or one that froze the remote's view before
                // the news this dial was for. Dialing again once it is over
                // settles both, whatever finished in between.
                let streak = self
                    .redial_streaks
                    .entry((namespace, peer, callee))
                    .or_default();
                *streak = streak.saturating_add(1);
                if *streak > MAX_REDIALS {
                    debug!(?reason, "redials in a row exhausted");
                    return;
                }
                let delay = REDIAL_AFTER_ABORT.saturating_mul(
                    1_u32
                        .checked_shl(streak.saturating_sub(1))
                        .unwrap_or(u32::MAX),
                );
                let redial = ToLiveActor::Redial {
                    namespace,
                    peer,
                    callee,
                    reason,
                };
                let due = self.redials_due.entry(namespace).or_default();
                *due = due.saturating_add(1);
                let to_self = self.sync_actor_tx.clone();
                n0_future::task::spawn(async move {
                    n0_future::time::sleep(delay).await;
                    to_self.send(redial).await.ok();
                });
            }
            res => {
                self.on_sync_finished(
                    namespace,
                    peer,
                    callee,
                    Origin::Connect(reason),
                    session,
                    res.map_err(Into::into),
                )
                .await
            }
        }
    }

    #[instrument("accept", skip_all, fields(peer = %fmt_accept_peer(&res), namespace = %fmt_accept_namespace(&res)))]
    async fn on_sync_via_accept_finished(
        &mut self,
        caller: Identity,
        session: Option<SessionId>,
        res: Result<SyncFinished, AcceptError>,
    ) {
        match res {
            Ok(state) => {
                self.on_sync_finished(
                    state.namespace,
                    state.peer,
                    caller,
                    Origin::Accept,
                    session,
                    Ok(state),
                )
                .await
            }
            // Refused by us before the exchange took the pair's slot, which
            // an exchange of ours with the same counterpart may hold.
            Err(AcceptError::Abort { reason, .. }) => {
                debug!(?reason, "aborted by us");
            }
            Err(err) => {
                if let (Some(peer), Some(namespace)) = (err.peer(), err.namespace()) {
                    self.on_sync_finished(
                        namespace,
                        peer,
                        caller,
                        Origin::Accept,
                        session,
                        Err(anyhow::Error::from(err)),
                    )
                    .await;
                } else {
                    debug!(?err, "failed before reading the first message");
                }
            }
        }
    }

    async fn on_sync_finished(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        counterpart: Identity,
        origin: Origin,
        session: Option<SessionId>,
        result: Result<SyncFinished>,
    ) {
        match &result {
            Err(ref err) => {
                warn!(?origin, ?err, "sync failed");
            }
            Ok(ref details) => {
                self.redial_streaks.remove(&(namespace, peer, counterpart));
                info!(
                    sent = %details.outcome.num_sent,
                    recv = %details.outcome.num_recv,
                    t_connect = ?details.timings.connect,
                    t_process = ?details.timings.process,
                    "sync finished",
                );

                // register the peer as useful for the document
                //
                // This node's own id is never one: the table names nodes to
                // dial, and a dial that resolves here is an in-process one,
                // which the consumer asks for through its own path. Left in,
                // it would make every later start of this replica dial this
                // node again, converged or not.
                if peer != self.endpoint.id() {
                    if let Err(e) = self
                        .sync
                        .register_useful_peer(namespace, *peer.as_bytes())
                        .await
                    {
                        debug!(%e, "failed to register peer for document")
                    }
                }

                // Retry content that is still missing for this namespace: the peer we
                // just synced with is a fresh provider candidate. Entries whose records
                // arrive ahead of their content are parked in `missing_hashes` and are
                // otherwise unparked only by a best-effort gossip `ContentReady`
                // broadcast — if that one message is lost, the content would starve
                // until an unrelated insert. `start_download` skips content that
                // arrived in the meantime and dedupes in-flight downloads. Content
                // already being downloaded gets the peer registered as a provider,
                // and the download is retried once if it fails: the running attempt
                // snapshotted its provider set before this peer joined it.
                let queued: Vec<Hash> = self
                    .queued_hashes
                    .by_namespace
                    .get(&namespace)
                    .map(|hashes| hashes.iter().copied().collect())
                    .unwrap_or_default();
                let parked: Vec<Hash> = self
                    .missing_hashes
                    .iter()
                    .filter(|(ns, _)| *ns == namespace)
                    .map(|(_, hash)| *hash)
                    .collect();
                for hash in parked {
                    debug!(peer=%peer.fmt_short(), %hash, "retrying parked content");
                    self.start_download(namespace, hash, peer, true).await;
                }
                if let Some(met) = self.content_scans.get_mut(&namespace) {
                    *met = Some(peer);
                }
                for hash in queued {
                    debug!(peer=%peer.fmt_short(), %hash, "registering sync peer for queued content");
                    self.retry_after_failure.insert((namespace, hash));
                    self.start_download(namespace, hash, peer, true).await;
                }

                // broadcast a sync report to our neighbors, but only if we received new entries.
                if details.outcome.num_recv > 0 {
                    info!("broadcast sync report to neighbors");
                    match details
                        .outcome
                        .heads_received
                        .encode(Some(self.gossip.max_message_size()))
                    {
                        Err(err) => warn!(?err, "Failed to encode author heads for sync report"),
                        Ok(heads) => {
                            let report = SyncReport {
                                namespace,
                                heads,
                                identity: self.identity,
                            };
                            self.broadcast_neighbors(namespace, &Op::SyncReport(report))
                                .await;
                        }
                    }
                }
            }
        };

        let result_for_event = match &result {
            Ok(details) => Ok(details.into()),
            Err(err) => Err(err.to_string()),
        };

        let finished = self.state.finish(&namespace, peer, counterpart, &origin);
        // A dial the tie-break superseded leaves the held dials to the
        // exchange that took its slot over.
        if !self.state.is_running(&namespace, (peer, counterpart)) {
            self.release_held(namespace, peer, counterpart);
        }
        let Some((started, resync)) = finished else {
            return;
        };

        let ev = SyncEvent {
            peer,
            counterpart,
            origin,
            result: result_for_event,
            finished: SystemTime::now(),
            started,
            session,
        };
        self.subscribers
            .send(&namespace, Event::SyncFinished(ev))
            .await;

        // Check if there are queued pending content hashes for this namespace.
        // If hashes are pending, mark this namespace to be eglible for a PendingContentReady event once all
        // pending hashes have completed downloading.
        // If no hashes are pending, emit the PendingContentReady event right away. The next
        // PendingContentReady event may then only be emitted after the next sync completes.
        if self.queued_hashes.contains_namespace(&namespace) {
            self.state.set_may_emit_ready(&namespace, true);
        } else {
            self.subscribers
                .send(&namespace, Event::PendingContentReady)
                .await;
            self.state.set_may_emit_ready(&namespace, false);
        }

        if resync {
            self.sync_with_peer(namespace, peer, SyncReason::Resync);
        }
    }

    async fn broadcast_neighbors(&mut self, namespace: NamespaceId, op: &Op) {
        if !self.state.is_syncing(&namespace) {
            return;
        }

        let msg = match postcard::to_stdvec(op) {
            Ok(msg) => msg,
            Err(err) => {
                error!(?err, ?op, "Failed to serialize message:");
                return;
            }
        };
        // TODO: We should debounce and merge these neighbor announcements likely.
        self.gossip
            .broadcast_neighbors(&namespace, msg.into())
            .await;
    }

    /// Announce a locally inserted entry to neighbors as a content-free
    /// [`SyncReport`] — its author head only, never the entry.
    ///
    /// The head is exactly the delta a neighbor needs to see it has news
    /// ([`AuthorHeads::has_news_for`]) and pull the entry over a classified
    /// reconciliation; the topic therefore carries digests, not keys, hashes,
    /// or values. Mirrors the post-sync report path: one hop, cascaded by
    /// each puller re-announcing to its own neighbors.
    async fn broadcast_local_head(&mut self, namespace: NamespaceId, entry: &SignedEntry) {
        let mut author_heads = AuthorHeads::default();
        author_heads.insert(entry.author_bytes(), entry.timestamp());
        let heads = match author_heads.encode(Some(self.gossip.max_message_size())) {
            Ok(heads) => heads,
            Err(err) => {
                warn!(
                    ?err,
                    "Failed to encode author head for local-insert announce"
                );
                return;
            }
        };
        let report = SyncReport {
            namespace,
            heads,
            identity: self.identity,
        };
        self.broadcast_neighbors(namespace, &Op::SyncReport(report))
            .await;
    }

    async fn on_download_ready(
        &mut self,
        namespace: NamespaceId,
        hash: Hash,
        res: Result<(), anyhow::Error>,
    ) {
        let completed_namespaces = self.queued_hashes.remove_hash(&hash);
        debug!(namespace=%namespace.fmt_short(), success=res.is_ok(), completed_namespaces=completed_namespaces.len(), "download ready");
        if res.is_ok() {
            self.retry_after_failure.retain(|(_, h)| *h != hash);
            self.subscribers
                .send(&namespace, Event::ContentReady { hash })
                .await;
            // Inform our neighbors that we have new content ready.
            self.broadcast_neighbors(namespace, &Op::ContentReady(hash))
                .await;
        } else {
            self.missing_hashes.insert((namespace, hash));
            if self.retry_after_failure.remove(&(namespace, hash)) {
                // A provider was registered while the failed download was already
                // running with an older provider snapshot: retry once with the
                // enriched set.
                debug!(%hash, "retrying failed download with providers registered meanwhile");
                self.queue_download(namespace, hash, true).await;
            }
        }
        for namespace in completed_namespaces.iter() {
            if let Some(true) = self.state.may_emit_ready(namespace) {
                self.subscribers
                    .send(namespace, Event::PendingContentReady)
                    .await;
            }
        }
    }

    async fn on_neighbor_content_ready(
        &mut self,
        namespace: NamespaceId,
        node: EndpointId,
        hash: Hash,
    ) {
        self.start_download(namespace, hash, node, true).await;
    }

    #[instrument("on_sync_report", skip_all, fields(peer = %from.fmt_short(), namespace = %report.namespace.fmt_short()))]
    async fn on_sync_report(&mut self, from: PublicKey, report: SyncReport) {
        let namespace = report.namespace;
        if !self.state.is_syncing(&namespace) {
            return;
        }
        let heads = match AuthorHeads::decode(&report.heads) {
            Ok(heads) => heads,
            Err(err) => {
                warn!(?err, "failed to decode AuthorHeads");
                return;
            }
        };
        match self.sync.has_news_for_us(report.namespace, heads).await {
            Ok(Some(updated_authors)) => {
                info!(%updated_authors, "news reported: sync now");
                self.sync_with_reporter(report.namespace, from, report.identity);
            }
            Ok(None) => {
                debug!("no news reported: nothing to do");
            }
            Err(err) => {
                warn!("sync actor error: {err:?}");
            }
        }
    }

    async fn on_replica_event(&mut self, event: crate::Event) -> Result<()> {
        // Before anything else: a download this event queues completes after it.
        match &event {
            crate::Event::LocalInsert { namespace, entry } => {
                self.subscribers
                    .send_ordered(namespace, || Event::LocalInsert {
                        entry: entry.clone(),
                    })
                    .await
            }
            crate::Event::RemoteInsert {
                namespace,
                entry,
                from,
                ..
            } => {
                self.subscribers
                    .send_ordered(namespace, || Event::RemoteInsert {
                        from: *from,
                        entry: entry.clone(),
                    })
                    .await
            }
            crate::Event::Lagged { .. } => {}
        }
        match event {
            crate::Event::LocalInsert { namespace, entry } => {
                debug!(namespace=%namespace.fmt_short(), "replica event: LocalInsert");
                // A new entry was inserted locally. Announce the new author
                // head to neighbors — the entry itself never rides the topic
                // (content-free swarm): a broadcast is relayed by every
                // member and cannot be filtered per recipient, so gossip
                // carries only "I have news" and the content flows over the
                // reconciliation the announce triggers. The announce is one
                // hop and cascades exactly like the post-sync report below:
                // a neighbor that pulls the entry re-announces to its own
                // neighbors, so a whole-topic broadcast (whose relays would
                // forward the head without holding the content the pull
                // then asks them for) is deliberately not used.
                if self.state.is_syncing(&namespace) {
                    self.broadcast_local_head(namespace, &entry).await;
                }
                // A node's own gossip broadcast never reaches its other
                // subscribers, so an identity co-located with this one is told
                // here or on the periodic pass (ADR-0013). Content-free like
                // the broadcast: what the co-located identity obtains comes
                // through the session its reconcile opens, and its filter.
                self.ask_co_located(crate::engine::CoLocatedRequest::Announce {
                    namespace,
                    writer: self.identity,
                });
            }
            crate::Event::RemoteInsert {
                namespace,
                entry,
                from,
                should_download,
                remote_content_status,
            } => {
                debug!(namespace=%namespace.fmt_short(), "replica event: RemoteInsert");
                // A new entry was inserted from initial sync or gossip. Queue downloading the
                // content.
                if should_download {
                    let hash = entry.content_hash();
                    // From the insert on: a collection read that missed the
                    // entry would sweep its payload, landed or lying orphaned.
                    if self.held_payloads.wants(&hash) {
                        if let Ok(tag) = self
                            .bao_store
                            .tags()
                            .temp_tag(HashAndFormat::raw(hash))
                            .await
                        {
                            self.held_payloads.hold(tag);
                        }
                    }
                    if matches!(remote_content_status, ContentStatus::Complete) {
                        let node_id = PublicKey::from_bytes(&from)?;
                        self.start_download(namespace, hash, node_id, false).await;
                    } else {
                        self.missing_hashes.insert((namespace, hash));
                    }
                }
            }
            // The live actor's subscription is `Delivery::Blocking`: nothing
            // is dropped on the way here.
            crate::Event::Lagged { .. } => {}
        }

        Ok(())
    }

    async fn start_download(
        &mut self,
        namespace: NamespaceId,
        hash: Hash,
        node: PublicKey,
        only_if_missing: bool,
    ) {
        self.hash_providers
            .0
            .lock()
            .expect("poisoned")
            .entry(hash)
            .or_default()
            .insert(node);
        self.queue_download(namespace, hash, only_if_missing).await;
    }

    /// Queue a download for `hash` from the providers registered so far, unless the
    /// content is already complete or a download is already running.
    async fn queue_download(&mut self, namespace: NamespaceId, hash: Hash, only_if_missing: bool) {
        let entry_status = self.bao_store.blobs().status(hash).await;
        if matches!(entry_status, Ok(BlobStatus::Complete { .. })) {
            self.missing_hashes.remove(&(namespace, hash));
            return;
        }
        if self.queued_hashes.contains_hash(&hash) {
            self.queued_hashes.insert(hash, namespace);
        } else if !only_if_missing || self.missing_hashes.contains(&(namespace, hash)) {
            let req = DownloadRequest::new(
                HashAndFormat::raw(hash),
                self.hash_providers.clone(),
                SplitStrategy::None,
            );
            let handle = self.downloader.download_with_opts(req);

            self.queued_hashes.insert(hash, namespace);
            self.missing_hashes.remove(&(namespace, hash));
            self.download_tasks.spawn(async move {
                (
                    namespace,
                    hash,
                    handle.await.map_err(|e| anyhow::anyhow!(e)),
                )
            });
        }
    }

    /// The accept decision: the engine's own session state first, then the
    /// consumer's access provider. A denial is indistinguishable from the
    /// namespace not being hosted.
    fn accept_callback(
        &self,
    ) -> impl Fn(NamespaceId, Identity, Identity, PublicKey) -> n0_future::future::Boxed<AcceptOutcome>
           + Clone
           + use<> {
        let to_actor_tx = self.sync_actor_tx.clone();
        let session_access = self.session_access.clone();
        move |namespace, identity, caller, peer| {
            let to_actor_tx = to_actor_tx.clone();
            let session_access = session_access.clone();
            async move {
                // Rights before state: the identity and the namespace are the
                // caller's own word, and asking the actor first would let a
                // caller the provider denies leave a slot of its choosing
                // behind on every attempt.
                let admitted = session_access(
                    namespace,
                    identity,
                    caller,
                    peer,
                    crate::filter::SessionRole::Accept,
                )
                .await;
                let (egress, ingest, rests_on) = match admitted {
                    crate::filter::SessionAccess::Allow {
                        egress,
                        ingest,
                        rests_on,
                    } => (egress, ingest, rests_on),
                    // The one uniform refusal.
                    crate::filter::SessionAccess::Deny => {
                        return AcceptOutcome::Reject(AbortReason::NotFound)
                    }
                };
                let recheck = crate::net::Recheck {
                    rests_on,
                    ask: Arc::new(move || {
                        session_access(
                            namespace,
                            identity,
                            caller,
                            peer,
                            crate::filter::SessionRole::Accept,
                        )
                    }),
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                to_actor_tx
                    .send(ToLiveActor::AcceptSyncRequest {
                        namespace,
                        peer,
                        caller,
                        reply: reply_tx,
                    })
                    .await
                    .ok();
                match reply_rx.await {
                    Ok(AcceptOutcome::Allow { .. }) => AcceptOutcome::Allow {
                        filter: egress,
                        ingest,
                        recheck: Some(recheck),
                    },
                    Ok(reject) => reject,
                    Err(err) => {
                        warn!(
                            "accept request callback failed to retrieve reply from actor: {err:?}"
                        );
                        AcceptOutcome::Reject(AbortReason::InternalServerError)
                    }
                }
            }
            .boxed()
        }
    }

    /// An engine that is the whole node's docs handler: read the first
    /// message here, then serve it. A node of several identities dispatches
    /// before this point ([`crate::protocol::Docs`]).
    #[instrument("accept", skip_all)]
    /// Serve a session whose first message named this engine's identity.
    #[instrument("accept", skip_all)]
    pub async fn handle_session(
        &mut self,
        conn: iroh::endpoint::Connection,
        opening: SessionOpening<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    ) {
        let (namespace, peer, caller) = (opening.namespace(), opening.peer(), opening.caller());
        let session = session_id(&conn);
        let accept_request_cb = self.accept_callback();
        let sync = self.sync.handle().clone();
        let metrics = self.metrics.clone();
        self.running_sync_accept.spawn(
            async move {
                let res = handle_session(sync, opening, accept_request_cb, Some(&metrics)).await;
                // The connection outlives the session it carries: dropping
                // it at the dispatcher would cut the streams mid-exchange.
                drop(conn);
                (namespace, peer, caller, session, res)
            }
            .instrument(Span::current()),
        );
    }

    /// The serving half of a session between two identities of this node.
    #[instrument("accept-in-process", skip_all)]
    fn accept_in_process(&mut self, opening: SessionOpening<InProcessRecv, InProcessSend>) {
        let (namespace, peer, caller) = (opening.namespace(), opening.peer(), opening.caller());
        let accept_request_cb = self.accept_callback();
        let sync = self.sync.handle().clone();
        let metrics = self.metrics.clone();
        self.running_sync_accept.spawn(
            async move {
                let res =
                    handle_in_process_session(sync, opening, accept_request_cb, Some(&metrics))
                        .await;
                (namespace, peer, caller, None, res)
            }
            .instrument(Span::current()),
        );
    }

    pub fn accept_sync_request(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        caller: Identity,
    ) -> AcceptOutcome {
        self.state
            .accept_request(&self.endpoint.id(), self.identity, &namespace, peer, caller)
    }
}

/// Event emitted when a sync operation completes
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SyncEvent {
    /// Peer we synced with
    pub peer: PublicKey,
    /// The identity the peer's side of the session was: the one a dial
    /// named, the one an accept's caller acted for. One node id hosts
    /// several identities (ADR-0013).
    pub counterpart: Identity,
    /// Origin of the sync exchange
    pub origin: Origin,
    /// Timestamp when the sync finished
    pub finished: SystemTime,
    /// Timestamp when the sync started
    pub started: SystemTime,
    /// Result of the sync operation
    pub result: std::result::Result<SyncDetails, String>,
    /// The id both ends of a session over a network connection report;
    /// `None` in process and for a dial that never connected.
    pub session: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SyncDetails {
    /// Number of entries received
    pub entries_received: usize,
    /// Number of entries sent
    pub entries_sent: usize,
}

impl From<&SyncFinished> for SyncDetails {
    fn from(value: &SyncFinished) -> Self {
        Self {
            entries_received: value.outcome.num_recv,
            entries_sent: value.outcome.num_sent,
        }
    }
}

/// The product's subscribers, which take inserts from the replica itself,
/// and the ordered ones, which take them from this actor.
#[derive(Debug, Default)]
struct SubscribersMap {
    all: HashMap<NamespaceId, Subscribers<Event>>,
    ordered: HashMap<NamespaceId, Subscribers<Event>>,
}

impl SubscribersMap {
    fn subscribe(&mut self, namespace: NamespaceId, sender: async_channel::Sender<Event>) {
        self.all
            .entry(namespace)
            .or_default()
            .subscribe(sender, Delivery::Lossy);
    }

    fn observe(&mut self, namespace: NamespaceId, sender: async_channel::Sender<Event>) {
        self.ordered
            .entry(namespace)
            .or_default()
            .subscribe(sender, Delivery::Lossy);
    }

    fn unobserve(&mut self, namespace: NamespaceId, sender: &async_channel::Sender<Event>) {
        if let Some(subscribers) = self.ordered.get_mut(&namespace) {
            subscribers.unsubscribe(sender);
        }
    }

    async fn send(&mut self, namespace: &NamespaceId, event: Event) {
        debug!(namespace=%namespace.fmt_short(), %event, "emit event");
        for map in [&mut self.all, &mut self.ordered] {
            if let Some(subscribers) = map.get_mut(namespace) {
                subscribers.send(event.clone()).await;
                if subscribers.is_empty() {
                    map.remove(namespace);
                }
            }
        }
    }

    /// Builds the event only for a namespace with an ordered subscriber.
    async fn send_ordered(&mut self, namespace: &NamespaceId, event: impl FnOnce() -> Event) {
        if let Some(subscribers) = self.ordered.get_mut(namespace) {
            subscribers.send_with(event).await;
            if subscribers.is_empty() {
                self.ordered.remove(namespace);
            }
        }
    }

    fn remove(&mut self, namespace: &NamespaceId) {
        self.all.remove(namespace);
        self.ordered.remove(namespace);
    }

    fn clear(&mut self) {
        self.all.clear();
        self.ordered.clear();
    }
}

#[derive(Debug, Default)]
struct QueuedHashes {
    by_hash: HashMap<Hash, HashSet<NamespaceId>>,
    by_namespace: HashMap<NamespaceId, HashSet<Hash>>,
}

#[derive(Debug, Clone, Default)]
struct ProviderNodes(Arc<std::sync::Mutex<HashMap<Hash, HashSet<EndpointId>>>>);

impl ContentDiscovery for ProviderNodes {
    fn find_providers(&self, hash: HashAndFormat) -> n0_future::stream::Boxed<EndpointId> {
        let nodes = self
            .0
            .lock()
            .expect("poisoned")
            .get(&hash.hash)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        Box::pin(n0_future::stream::iter(nodes))
    }
}

impl QueuedHashes {
    fn insert(&mut self, hash: Hash, namespace: NamespaceId) {
        self.by_hash.entry(hash).or_default().insert(namespace);
        self.by_namespace.entry(namespace).or_default().insert(hash);
    }

    /// Remove a hash from the set of queued hashes.
    ///
    /// Returns a list of namespaces that are now complete (have no queued hashes anymore).
    fn remove_hash(&mut self, hash: &Hash) -> Vec<NamespaceId> {
        let namespaces = self.by_hash.remove(hash).unwrap_or_default();
        let mut removed_namespaces = vec![];
        for namespace in namespaces {
            if let Some(hashes) = self.by_namespace.get_mut(&namespace) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.by_namespace.remove(&namespace);
                    removed_namespaces.push(namespace);
                }
            }
        }
        removed_namespaces
    }

    fn contains_hash(&self, hash: &Hash) -> bool {
        self.by_hash.contains_key(hash)
    }

    fn contains_namespace(&self, namespace: &NamespaceId) -> bool {
        self.by_namespace.contains_key(namespace)
    }
}

/// The content of every entry of `namespace` the download policy wants and
/// `blobs` does not hold whole.
async fn missing_content(
    sync: &SyncHandle,
    blobs: &Store,
    namespace: NamespaceId,
) -> Result<Vec<Hash>> {
    let policy = sync.get_download_policy(namespace).await?;
    let (tx, mut rx) = irpc::channel::mpsc::channel(64);
    sync.get_many(namespace, crate::store::Query::all().into(), tx)
        .await?;
    let mut missing = Vec::new();
    while let Some(entry) = rx.recv().await? {
        let entry = entry?;
        if entry.content_len() == 0 || !policy.matches(entry.entry()) {
            continue;
        }
        let hash = entry.content_hash();
        let status = blobs.blobs().status(hash).await;
        if !matches!(status, Ok(BlobStatus::Complete { .. })) {
            missing.push(hash);
        }
    }
    Ok(missing)
}

fn fmt_accept_peer(res: &Result<SyncFinished, AcceptError>) -> String {
    match res {
        Ok(res) => res.peer.fmt_short().to_string(),
        Err(err) => err
            .peer()
            .map(|x| x.fmt_short().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

fn fmt_accept_namespace(res: &Result<SyncFinished, AcceptError>) -> String {
    match res {
        Ok(res) => res.namespace.fmt_short(),
        Err(err) => err
            .namespace()
            .map(|x| x.fmt_short())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sync_remove() {
        let pk = PublicKey::from_bytes(&[1; 32]).unwrap();
        let (a_tx, a_rx) = async_channel::unbounded();
        let (b_tx, b_rx) = async_channel::unbounded();
        let mut subscribers = Subscribers::default();
        subscribers.subscribe(a_tx, Delivery::Lossy);
        subscribers.subscribe(b_tx, Delivery::Lossy);
        drop(a_rx);
        drop(b_rx);
        subscribers.send(Event::NeighborUp(pk)).await;
    }

    /// A peer of one replica is dialed as every identity it is known by, and
    /// a session's identity is known only while that session runs: two
    /// sessions of one identity both have to end before it is forgotten, and
    /// a contact the consumer stated outlives all of them.
    #[test]
    fn a_session_identity_lives_as_long_as_its_session_and_a_stated_one_outlives_it() {
        let stated = Identity::from_bytes([1; 32]);
        let named = Identity::from_bytes([2; 32]);
        let mut identities = PeerIdentities::default();
        identities.stated.insert(stated);

        identities.enter(named);
        identities.enter(named);
        assert_eq!(identities.all().collect::<Vec<_>>(), vec![stated, named]);

        // One of the two sessions ends: the identity is still named by the other.
        identities.leave(named);
        assert_eq!(identities.all().collect::<Vec<_>>(), vec![stated, named]);

        identities.leave(named);
        assert_eq!(
            identities.all().collect::<Vec<_>>(),
            vec![stated],
            "the identity outlived the sessions that named it"
        );
        assert!(
            !identities.is_empty(),
            "a stated contact was dropped with the sessions"
        );

        // A release with nothing to release leaves the stated one alone.
        identities.leave(named);
        assert_eq!(identities.all().collect::<Vec<_>>(), vec![stated]);
    }
}
