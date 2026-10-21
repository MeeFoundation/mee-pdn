#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use iroh::{address_lookup::memory::MemoryLookup, Endpoint, EndpointAddr, PublicKey};
use iroh_blobs::{
    api::{blobs::BlobStatus, remote::GetProgressItem, Store},
    get::{
        fsm::{AtBlobHeaderNextError, DecodeError},
        GetError,
    },
    util::connection_pool::{self, ConnectionPool},
    Hash, HashAndFormat,
};
use iroh_gossip::net::Gossip;
use n0_future::{
    task::JoinSet,
    time::{Duration, Instant, SystemTime},
    FutureExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{self, mpsc, oneshot};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};

use self::{
    draining::DrainingSync,
    fetches::{Fetches, Outcome, Steps, NO_PROGRESS},
};
use super::state::{NamespaceStates, Origin, SyncReason};
use crate::{
    actor::SyncHandle,
    engine::gossip::GossipState,
    net::{
        connect_and_sync_asking, handle_in_process_session, handle_session, session_id,
        AbortReason, AcceptError, AcceptOutcome, ConnectError, SessionId, SessionOpening,
        SyncFinished,
    },
    subscribers::{Delivery, LagNotice, Subscribers},
    AuthorHeads, Contact, ContentStatus, Identity, NamespaceId, SignedEntry,
};

mod draining;
mod fetches;

/// The stream halves an in-process session runs over: a pipe, since iroh
/// refuses a connection to this endpoint's own id.
pub(crate) type InProcessRecv = tokio::io::ReadHalf<tokio::io::DuplexStream>;
/// The writing half of an in-process session's pipe.
pub(crate) type InProcessSend = tokio::io::WriteHalf<tokio::io::DuplexStream>;

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
pub(crate) struct SyncReport {
    namespace: NamespaceId,
    /// Encoded [`AuthorHeads`]
    heads: Vec<u8>,
    /// The identity the sender's replica answers as.
    identity: Identity,
}

/// Messages to the live actor
#[derive(derive_more::Debug, strum::Display)]
pub(crate) enum ToLiveActor {
    StartSync {
        namespace: NamespaceId,
        peers: Vec<Contact>,
        /// The recorded peers dialed beside `peers`: every one the store
        /// holds for the replica when `None`.
        recorded: Option<Vec<PublicKey>>,
        /// Whom a peer no contact or session named is dialed as; `None`
        /// keeps what an earlier start stated, else this engine's identity.
        default_identity: Option<Identity>,
        /// Whether to join the replica's gossip swarm. A replica held out of
        /// its swarm — the data layer's pod tombstone — syncs without
        /// joining it.
        join_gossip: bool,
        /// An ordered subscriber, registered before the replica is opened
        /// for sync and before the first dial.
        #[debug("observer")]
        observer: Option<async_channel::Sender<Event>>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<anyhow::Result<()>>,
    },
    LeaveGossip {
        namespace: NamespaceId,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<anyhow::Result<()>>,
    },
    DropReplica {
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
    #[cfg(feature = "observation")]
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
    /// A dial the remote aborted as already syncing, or one that could not
    /// reach a gossip neighbor, asked for again; `retry` names the latter's
    /// series.
    Redial {
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
        retry: Option<u64>,
    },
    /// The pause of `provider`'s series `series` is over.
    ResumeFetches {
        provider: PublicKey,
        series: u64,
    },
    /// The exchanges of `namespaces` running, held behind another's, or
    /// waiting to redial.
    SyncsInFlight {
        namespaces: Vec<NamespaceId>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<usize>,
    },
    /// What [`ToLiveActor::SyncsInFlight`] counts, each item named.
    /// [`LiveActor::fetches_report`].
    FetchesReport {
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<String>,
    },
    SyncsInFlightReport {
        namespaces: Vec<NamespaceId>,
        #[debug("onsehot::Sender")]
        reply: sync::oneshot::Sender<String>,
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
    /// `generation` names the subscription whose receive loop saw it: one
    /// sent before a quit is read after it, and must not make a neighbor of
    /// a topic no longer joined.
    NeighborUp {
        namespace: NamespaceId,
        peer: PublicKey,
        generation: u64,
    },
    NeighborDown {
        namespace: NamespaceId,
        peer: PublicKey,
        generation: u64,
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
type FetchRes = (PublicKey, Hash, Outcome);

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

/// The wait before a dial that failed toward a gossip neighbor is tried
/// again, and a provider's first pause; each further wait doubles, up to
/// [`UNREACHED_RETRY_MAX`].
const UNREACHED_RETRY_FIRST: Duration = Duration::from_secs(1);
const UNREACHED_RETRY_MAX: Duration = Duration::from_secs(16);

/// How long after its first failure such a dial or provider is still tried:
/// past the minute in which a peer restarted on a new port opens another
/// connection here, which is when iroh selects its path again.
const UNREACHED_RETRY_FOR: Duration = Duration::from_secs(120);

/// The tries of one dial or download toward a gossip neighbor. It arms one
/// timer at a time, and once run out it stays until what ends it, so a
/// failure from another trigger neither adds a timer nor starts it again.
#[derive(Debug)]
struct RetrySeries {
    /// Names the series in its timer's message: a timer outliving its
    /// series must not disarm the next one.
    id: u64,
    since: n0_future::time::Instant,
    wait: Duration,
    armed: bool,
    exhausted: bool,
}

impl RetrySeries {
    fn new(id: u64) -> Self {
        Self {
            id,
            since: n0_future::time::Instant::now(),
            wait: UNREACHED_RETRY_FIRST,
            armed: false,
            exhausted: false,
        }
    }

    /// The wait before the next try, arming the series; `None` while a try
    /// is armed, and once the series has run its span.
    fn arm(&mut self) -> Option<Duration> {
        if self.armed || self.exhausted {
            return None;
        }
        if self.since.elapsed() >= UNREACHED_RETRY_FOR {
            self.exhausted = true;
            return None;
        }
        let wait = self.wait;
        self.wait = self.wait.saturating_mul(2).min(UNREACHED_RETRY_MAX);
        self.armed = true;
        Some(wait)
    }

    /// The armed try of series `id` has fired.
    fn fired(&mut self, id: u64) {
        if self.id == id {
            self.armed = false;
        }
    }
}

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
pub(crate) struct LiveActor {
    /// Receiver for actor messages.
    inbox: mpsc::Receiver<ToLiveActor>,
    sync: DrainingSync,
    endpoint: Endpoint,
    bao_store: Store,
    held_payloads: Arc<super::held::HeldPayloads>,
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
    /// Running fetches, each from one provider.
    download_tasks: JoinSet<FetchRes>,
    /// Which provider each wanted payload is fetched from, and when.
    fetches: Fetches,
    pool: ConnectionPool,
    /// Payloads [`Fetches`] left with no provider during this turn of the
    /// loop, parked at its end.
    unrouted: Vec<Hash>,
    /// Wanted payloads no provider is registered for, keyed by the
    /// namespace whose entry wants them (the namespace drives retries on
    /// sync-finished and keeps `PendingContentReady` attribution correct).
    missing_hashes: HashSet<(NamespaceId, Hash)>,
    /// Per namespace whose content scan runs, the peer of the last session
    /// that went through meanwhile: its retry of parked content ran before
    /// the scan parked anything.
    content_scans: HashMap<NamespaceId, Option<PublicKey>>,
    /// Wanted payloads with a provider to fetch them from, in flight or
    /// waiting in [`Fetches`].
    queued_hashes: QueuedHashes,

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
    /// Dials that failed to connect to a gossip neighbor, until an exchange
    /// with the pair ends any other way or the neighbor leaves; a series
    /// run out stays until then.
    unreached_dials: HashMap<(NamespaceId, PublicKey, Identity), RetrySeries>,
    /// Pairs whose last dial the remote refused, until a session with the
    /// pair goes through: a dial of theirs crossing one of ours takes the
    /// pair over.
    refused_dials: HashSet<(NamespaceId, PublicKey, Identity)>,
    /// The id the next [`RetrySeries`] takes.
    next_retry_series: u64,
    /// The topics each peer is a gossip neighbor in: a connection with a
    /// neighbor stands, so it is up.
    neighbors: HashMap<PublicKey, HashSet<NamespaceId>>,
    /// In-process sessions opened, so a pass over a quiet pair can be
    /// shown to open none.
    in_process_sessions: Arc<AtomicU64>,
    /// Where a local write and a contact naming this node are handed to
    /// the node, which alone knows its other identities; `None` hands them
    /// to nobody.
    co_located: Option<crate::engine::CoLocatedRequests>,
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
        fetches_per_provider: usize,
        inbox: mpsc::Receiver<ToLiveActor>,
        sync_actor_tx: mpsc::Sender<ToLiveActor>,
        session_access: crate::filter::SessionAccessProvider,
        identity: Identity,
        in_process_sessions: Arc<AtomicU64>,
        co_located: Option<crate::engine::CoLocatedRequests>,
    ) -> Result<Self> {
        let gossip_state = GossipState::new(gossip, sync_actor_tx.clone());
        let memory_lookup = MemoryLookup::new();
        endpoint.address_lookup()?.add(memory_lookup.clone());
        let pool = ConnectionPool::new(
            endpoint.clone(),
            iroh_blobs::ALPN,
            connection_pool::Options::default(),
        );
        Ok(Self {
            inbox,
            sync: DrainingSync::new(sync),
            endpoint,
            memory_lookup,
            gossip: gossip_state,
            bao_store,
            held_payloads,
            sync_actor_tx,
            running_sync_connect: Default::default(),
            running_sync_accept: Default::default(),
            subscribers: Default::default(),
            download_tasks: Default::default(),
            fetches: Fetches::new(fetches_per_provider),
            pool,
            unrouted: Vec::new(),
            state: Default::default(),
            missing_hashes: Default::default(),
            content_scans: Default::default(),
            queued_hashes: Default::default(),
            session_access,
            identity,
            peer_identities: Default::default(),
            default_identities: Default::default(),
            prerequisites: Default::default(),
            held_dials: Default::default(),
            redials_due: Default::default(),
            redial_streaks: Default::default(),
            unreached_dials: Default::default(),
            refused_dials: Default::default(),
            next_retry_series: 0,
            neighbors: Default::default(),
            in_process_sessions,
            co_located,
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
            tokio::select! {
                biased;
                // First: behind a busy inbox a landed payload's ContentReady waits for seconds, and
                // completions are bounded by the downloads this actor started.
                Some(res) = self.download_tasks.join_next(), if !self.download_tasks.is_empty() => {
                    trace!(?i, "tick: pending_downloads");
                    let (provider, hash, outcome) = res.context("pending_downloads closed")?;
                    self.on_fetch_finished(provider, hash, outcome).await;
                }
                msg = self.inbox.recv() => {
                    let msg = msg.context("to_actor closed")?;
                    trace!(?i, %msg, "tick: to_actor");
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
                    let event = event.context("replica_events closed")?;
                    if let Err(err) = self.on_replica_event(event).await {
                        error!(?err, "Failed to process replica event");
                    }
                }
                Some(res) = self.running_sync_connect.join_next(), if !self.running_sync_connect.is_empty() => {
                    trace!(?i, "tick: running_sync_connect");
                    let (namespace, peer, callee, reason, session, res) = res.context("running_sync_connect closed")?;
                    self.on_sync_via_connect_finished(namespace, peer, callee, reason, session, res).await;

                }
                Some(res) = self.running_sync_accept.join_next(), if !self.running_sync_accept.is_empty() => {
                    trace!(?i, "tick: running_sync_accept");
                    let (namespace, peer, caller, session, res) = res.context("running_sync_accept closed")?;
                    self.release_peer_identity(namespace, peer, caller);
                    self.on_sync_via_accept_finished(caller, session, res).await;
                }
                res = self.gossip.next_ended(), if !self.gossip.is_empty() => {
                    match res {
                        Ok(namespace) => self.forget_topic_neighbors(namespace),
                        Err(error) => warn!(?error, "gossip state failed"),
                    }
                }
            }
            self.park_unrouted().await;
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
            ToLiveActor::NeighborUp {
                namespace,
                peer,
                generation,
            } => {
                if !self.gossip.is_current(&namespace, generation) {
                    debug!(peer = %peer.fmt_short(), namespace = %namespace.fmt_short(), "neighbor up from a subscription gone");
                    return Ok(true);
                }
                debug!(peer = %peer.fmt_short(), namespace = %namespace.fmt_short(), "neighbor up");
                self.neighbors.entry(peer).or_default().insert(namespace);
                self.sync_with_peer(namespace, peer, SyncReason::NewNeighbor);
                self.subscribers
                    .send(&namespace, Event::NeighborUp(peer))
                    .await;
            }
            ToLiveActor::NeighborDown {
                namespace,
                peer,
                generation,
            } => {
                if !self.gossip.is_current(&namespace, generation) {
                    return Ok(true);
                }
                debug!(peer = %peer.fmt_short(), namespace = %namespace.fmt_short(), "neighbor down");
                self.forget_neighbor(namespace, peer);
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
            ToLiveActor::LeaveGossip { namespace, reply } => {
                self.leave_gossip(namespace);
                reply.send(Ok(())).ok();
            }
            ToLiveActor::DropReplica { namespace, reply } => {
                let res = match self.leave(namespace, true).await {
                    Ok(()) => self.sync.drop_replica(namespace).await,
                    Err(err) => Err(err),
                };
                reply.send(res).ok();
            }
            ToLiveActor::Subscribe {
                namespace,
                sender,
                reply,
            } => {
                self.subscribers.subscribe(namespace, sender);
                reply.send(Ok(())).ok();
            }
            #[cfg(feature = "observation")]
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
                retry,
            } => {
                if let Some(due) = self.redials_due.get_mut(&namespace) {
                    *due = due.saturating_sub(1);
                }
                if let Some(id) = retry {
                    if let Some(series) = self.unreached_dials.get_mut(&(namespace, peer, callee)) {
                        series.fired(id);
                    }
                }
                self.sync_with_identity(namespace, peer, callee, reason);
            }
            ToLiveActor::ResumeFetches { provider, series } => {
                let steps = self.fetches.resume(provider, series);
                self.apply_fetches(steps);
            }
            ToLiveActor::SyncsInFlight { namespaces, reply } => {
                reply.send(self.syncs_in_flight(&namespaces)).ok();
            }
            ToLiveActor::FetchesReport { reply } => {
                reply.send(self.fetches_report()).ok();
            }
            ToLiveActor::SyncsInFlightReport { namespaces, reply } => {
                reply.send(self.syncs_in_flight_report(&namespaces)).ok();
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
                self.register_provider(namespace, hash, node);
            }
        };
        Ok(true)
    }

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
    /// blob store does not hold whole, and
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
            if self.queued_hashes.contains_hash(&hash) {
                self.queued_hashes.insert(hash, namespace);
            } else {
                self.missing_hashes.insert((namespace, hash));
            }
            if let Some(peer) = met {
                self.register_provider(namespace, hash, peer);
            }
        }
    }

    async fn leave(
        &mut self,
        namespace: NamespaceId,
        kill_subscribers: bool,
    ) -> anyhow::Result<()> {
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
            self.unreached_dials
                .retain(|(tracked, _peer, _callee), _series| *tracked != namespace);
            self.refused_dials
                .retain(|(tracked, _peer, _callee)| *tracked != namespace);
            for hash in self.queued_hashes.remove_namespace(&namespace) {
                self.fetches.unwant(&hash);
            }
            self.missing_hashes
                .retain(|(tracked, _hash)| *tracked != namespace);
            self.sync.set_sync(namespace, false).await?;
            self.sync.unsubscribe(namespace).await?;
            self.sync.close(namespace).await?;
            self.leave_gossip(namespace);
        }
        if kill_subscribers {
            self.subscribers.remove(&namespace);
        }
        for (first, peer, counterpart) in held_on_it {
            self.release_held(first, peer, counterpart);
        }
        Ok(())
    }

    /// The exchanges of `namespaces` running, the dials held behind one of
    /// them or held on one, and the redials due.
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

    /// What the fetches stand at, and the payloads parked.
    fn fetches_report(&self) -> String {
        format!(
            "{}; {} parked",
            self.fetches.report(),
            self.missing_hashes.len()
        )
    }

    fn syncs_in_flight_report(&self, namespaces: &[NamespaceId]) -> String {
        let mut lines = Vec::new();
        for namespace in namespaces {
            for running in self.state.running_described(namespace) {
                lines.push(format!("{} running: {running}", namespace.fmt_short()));
            }
            if let Some(due) = self.redials_due.get(namespace).filter(|due| **due > 0) {
                lines.push(format!("{} redials due: {due}", namespace.fmt_short()));
            }
        }
        for ((first, peer, counterpart), dials) in &self.held_dials {
            for dial in dials {
                let held = dial.namespace();
                if namespaces.contains(first) || namespaces.contains(&held) {
                    lines.push(format!(
                        "{} held behind {} with {} as {}",
                        held.fmt_short(),
                        first.fmt_short(),
                        peer.fmt_short(),
                        counterpart.fmt_short()
                    ));
                }
            }
        }
        lines.join("; ")
    }

    /// The stated identities of `namespace`'s peers become `contacts`'
    /// alone; the ones a running session named stay until it ends.
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
        self.forget_topic_neighbors(namespace);
    }

    fn forget_topic_neighbors(&mut self, namespace: NamespaceId) {
        let peers: Vec<PublicKey> = self.neighbors.keys().copied().collect();
        for peer in peers {
            self.forget_neighbor(namespace, peer);
        }
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

        // tell gossip to join — unless the replica is held out of its
        // swarm: reconciliation is then its only data path, and the direct
        // syncs below still run.
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
                self.unreached_dials.remove(&(namespace, peer, callee));
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
                self.redial_after(namespace, peer, callee, reason, delay, None);
            }
            res => {
                if matches!(res, Err(ConnectError::RemoteAbort(AbortReason::NotFound))) {
                    self.refused_dials.insert((namespace, peer, callee));
                }
                let unreached = matches!(res, Err(ConnectError::Connect { .. }));
                if !unreached {
                    // The pair was reached, whatever the exchange came to.
                    self.unreached_dials.remove(&(namespace, peer, callee));
                }
                self.on_sync_finished(
                    namespace,
                    peer,
                    callee,
                    Origin::Connect(reason),
                    session,
                    res.map_err(Into::into),
                )
                .await;
                if unreached {
                    self.retry_unreached_dial(namespace, peer, callee, reason)
                        .await;
                }
            }
        }
    }

    fn redial_after(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
        delay: Duration,
        retry: Option<u64>,
    ) {
        let redial = ToLiveActor::Redial {
            namespace,
            peer,
            callee,
            reason,
            retry,
        };
        let due = self.redials_due.entry(namespace).or_default();
        *due = due.saturating_add(1);
        let to_self = self.sync_actor_tx.clone();
        n0_future::task::spawn(async move {
            n0_future::time::sleep(delay).await;
            to_self.send(redial).await.ok();
        });
    }

    fn forget_neighbor(&mut self, namespace: NamespaceId, peer: PublicKey) {
        if let Some(topics) = self.neighbors.get_mut(&peer) {
            topics.remove(&namespace);
            if topics.is_empty() {
                self.neighbors.remove(&peer);
                self.end_retries_toward(peer);
            }
        }
    }

    /// A peer that is no neighbor any more ends the series toward it, so its
    /// next arrival starts a fresh one.
    fn end_retries_toward(&mut self, peer: PublicKey) {
        self.unreached_dials
            .retain(|(_namespace, toward, _callee), _series| *toward != peer);
        let steps = self.fetches.neighbor_gone(peer);
        self.apply_fetches(steps);
    }

    fn new_retry_series(&mut self) -> RetrySeries {
        let id = self.next_retry_series;
        self.next_retry_series = id.wrapping_add(1);
        RetrySeries::new(id)
    }

    /// Whether `peer` is a gossip neighbor in some topic of this engine: a
    /// connection with it stands, so it is up, and a dial toward it that
    /// cannot connect is going to a path iroh has yet to select again.
    fn is_neighbor(&self, peer: PublicKey) -> bool {
        self.neighbors.contains_key(&peer)
    }

    /// A dial that failed to connect, tried again while the peer is a
    /// neighbor and the series lasts: iroh keeps a restarted peer's old path
    /// selected until the peer opens another connection here.
    async fn retry_unreached_dial(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        callee: Identity,
        reason: SyncReason,
    ) {
        let key = (namespace, peer, callee);
        if !self.is_neighbor(peer) {
            self.unreached_dials.remove(&key);
            return;
        }
        let fresh = self.new_retry_series();
        let series = self.unreached_dials.entry(key).or_insert(fresh);
        let was_exhausted = series.exhausted;
        let Some(delay) = series.arm() else {
            if series.exhausted && !was_exhausted {
                info!(?reason, "dial tries toward a neighbor exhausted");
            }
            return;
        };
        let id = series.id;
        debug!(?delay, "dial could not reach a neighbor: trying again");
        self.redial_after(namespace, peer, callee, reason, delay, Some(id));
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
                    // The pair reached us, whatever the exchange came to.
                    self.unreached_dials.remove(&(namespace, peer, caller));
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
                self.refused_dials.remove(&(namespace, peer, counterpart));
                self.redial_streaks.remove(&(namespace, peer, counterpart));
                self.unreached_dials.remove(&(namespace, peer, counterpart));
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

                // Every payload the namespace waits for may come from the peer just synced
                // with: one parked because its entry came from a node lacking it would
                // otherwise wait on a best-effort gossip `ContentReady`.
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
                    self.register_provider(namespace, hash, peer);
                }
                if let Some(met) = self.content_scans.get_mut(&namespace) {
                    *met = Some(peer);
                }
                for hash in queued {
                    self.register_provider(namespace, hash, peer);
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

    async fn on_fetch_finished(&mut self, provider: PublicKey, hash: Hash, outcome: Outcome) {
        debug!(provider = %provider.fmt_short(), %hash, ?outcome, "fetch finished");
        let neighbor = self.is_neighbor(provider);
        let steps = self
            .fetches
            .finished(provider, hash, outcome, neighbor, Instant::now());
        match outcome {
            Outcome::Fetched => {
                let (wanting, emptied) = self.queued_hashes.take_hash(&hash);
                for namespace in wanting {
                    self.subscribers
                        .send(&namespace, Event::ContentReady { hash })
                        .await;
                    self.broadcast_neighbors(namespace, &Op::ContentReady(hash))
                        .await;
                }
                self.emit_pending_content_ready(emptied).await;
            }
            // Whole already, as an insert finding its payload held reports nothing.
            Outcome::Held => {
                let (_wanting, emptied) = self.queued_hashes.take_hash(&hash);
                self.emit_pending_content_ready(emptied).await;
            }
            Outcome::LocalFailure => {
                warn!(%hash, "a fetch failed on the node's own blob store");
            }
            _ => {}
        }
        self.apply_fetches(steps);
    }

    /// Records `provider` as holding `hash`; a payload `namespace` parked
    /// is wanted again once a provider holds it.
    fn register_provider(&mut self, namespace: NamespaceId, hash: Hash, provider: PublicKey) {
        let steps = self.fetches.register(hash, provider);
        self.apply_fetches(steps);
        if self.missing_hashes.remove(&(namespace, hash)) {
            self.want(namespace, hash);
        }
    }

    /// Wants `hash` for `namespace`: fetched from the providers registered
    /// for it, or parked while there is none.
    fn want(&mut self, namespace: NamespaceId, hash: Hash) {
        if self.fetches.is_wanted(&hash) {
            self.queued_hashes.insert(hash, namespace);
            return;
        }
        let (routed, steps) = self.fetches.want(hash);
        if routed {
            self.queued_hashes.insert(hash, namespace);
        } else {
            self.missing_hashes.insert((namespace, hash));
        }
        self.apply_fetches(steps);
    }

    /// Starts the fetches and times the pause `steps` name, and keeps what
    /// they unrouted for [`Self::park_unrouted`].
    fn apply_fetches(&mut self, steps: Steps) {
        for (provider, hash) in steps.start {
            let pool = self.pool.clone();
            let blobs = self.bao_store.clone();
            self.download_tasks.spawn(async move {
                let outcome = fetch(pool, blobs, provider, hash).await;
                (provider, hash, outcome)
            });
        }
        if let Some((provider, series, wait)) = steps.pause {
            debug!(provider = %provider.fmt_short(), ?wait, "fetches from a provider pause");
            let to_self = self.sync_actor_tx.clone();
            n0_future::task::spawn(async move {
                n0_future::time::sleep(wait).await;
                to_self
                    .send(ToLiveActor::ResumeFetches { provider, series })
                    .await
                    .ok();
            });
        }
        self.unrouted.extend(steps.unrouted);
    }

    /// Parks what this turn left with no provider, for every namespace that
    /// wants it and is still synced.
    async fn park_unrouted(&mut self) {
        if self.unrouted.is_empty() {
            return;
        }
        let mut emptied = Vec::new();
        for hash in std::mem::take(&mut self.unrouted) {
            // Wanted again by a provider registered later in the turn.
            if self.fetches.is_wanted(&hash) {
                continue;
            }
            let (wanting, done) = self.queued_hashes.take_hash(&hash);
            for namespace in wanting {
                if self.state.is_syncing(&namespace) {
                    self.missing_hashes.insert((namespace, hash));
                }
            }
            emptied.extend(done);
        }
        self.emit_pending_content_ready(emptied).await;
    }

    /// `PendingContentReady` for each of `namespaces`, which no longer wait
    /// for any payload, where a finished sync left it due.
    async fn emit_pending_content_ready(&mut self, namespaces: Vec<NamespaceId>) {
        for namespace in namespaces {
            if let Some(true) = self.state.may_emit_ready(&namespace) {
                self.subscribers
                    .send(&namespace, Event::PendingContentReady)
                    .await;
            }
        }
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
        #[cfg(feature = "observation")]
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
                remote_content_status,
            } => {
                debug!(namespace=%namespace.fmt_short(), "replica event: RemoteInsert");
                // A new entry was inserted from initial sync or gossip. Queue downloading the
                // content.
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
                    let steps = self.fetches.register(hash, node_id);
                    self.apply_fetches(steps);
                    self.missing_hashes.remove(&(namespace, hash));
                    self.want(namespace, hash);
                } else if self.queued_hashes.contains_hash(&hash) {
                    self.queued_hashes.insert(hash, namespace);
                } else {
                    self.missing_hashes.insert((namespace, hash));
                }
            }
            // The live actor's subscription is `Delivery::Blocking`: nothing
            // is dropped on the way here.
            crate::Event::Lagged { .. } => {}
        }

        Ok(())
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

    #[instrument("accept", skip_all)]
    /// Serve a session whose first message named this engine's identity.
    #[instrument("accept", skip_all)]
    pub(crate) async fn handle_session(
        &mut self,
        conn: iroh::endpoint::Connection,
        opening: SessionOpening<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    ) {
        let (namespace, peer, caller) = (opening.namespace(), opening.peer(), opening.caller());
        // One key export per connection, for the observation alone.
        let session = if cfg!(feature = "observation") {
            session_id(&conn)
        } else {
            None
        };
        let accept_request_cb = self.accept_callback();
        let sync = self.sync.handle().clone();
        self.running_sync_accept.spawn(
            async move {
                let res = handle_session(sync, opening, accept_request_cb).await;
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
        self.running_sync_accept.spawn(
            async move {
                let res = handle_in_process_session(sync, opening, accept_request_cb).await;
                (namespace, peer, caller, None, res)
            }
            .instrument(Span::current()),
        );
    }

    pub(crate) fn accept_sync_request(
        &mut self,
        namespace: NamespaceId,
        peer: PublicKey,
        caller: Identity,
    ) -> AcceptOutcome {
        let refused_ours = self.refused_dials.contains(&(namespace, peer, caller));
        self.state.accept_request(
            &self.endpoint.id(),
            self.identity,
            &namespace,
            peer,
            caller,
            refused_ours,
        )
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
    #[cfg(feature = "observation")]
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

    /// Removes `hash`, answering the namespaces that wanted it and those
    /// of them that now want nothing.
    fn take_hash(&mut self, hash: &Hash) -> (Vec<NamespaceId>, Vec<NamespaceId>) {
        let wanting: Vec<NamespaceId> = self
            .by_hash
            .get(hash)
            .map(|namespaces| namespaces.iter().copied().collect())
            .unwrap_or_default();
        let emptied = self.remove_hash(hash);
        (wanting, emptied)
    }

    /// Removes `namespace`, answering the hashes no other namespace wants.
    fn remove_namespace(&mut self, namespace: &NamespaceId) -> Vec<Hash> {
        let hashes = self.by_namespace.remove(namespace).unwrap_or_default();
        let mut unwanted = Vec::new();
        for hash in hashes {
            if let Some(namespaces) = self.by_hash.get_mut(&hash) {
                namespaces.remove(namespace);
                if namespaces.is_empty() {
                    self.by_hash.remove(&hash);
                    unwanted.push(hash);
                }
            }
        }
        unwanted
    }

    fn contains_hash(&self, hash: &Hash) -> bool {
        self.by_hash.contains_key(hash)
    }

    fn contains_namespace(&self, namespace: &NamespaceId) -> bool {
        self.by_namespace.contains_key(namespace)
    }
}

/// Fetches `hash` from `provider` alone, until it arrives, fails, or goes
/// [`NO_PROGRESS`] with nothing: the request's stream lives in this future,
/// so dropping it frees the stream (iroh-blobs-006).
async fn fetch(pool: ConnectionPool, blobs: Store, provider: PublicKey, hash: Hash) -> Outcome {
    let content = HashAndFormat::raw(hash);
    match blobs.remote().local(content).await {
        Ok(local) if local.is_complete() => return Outcome::Held,
        Ok(_) => {}
        Err(err) => {
            debug!(%hash, "the blob store cannot say what it holds: {err:#}");
            return Outcome::LocalFailure;
        }
    }
    let connection =
        match n0_future::time::timeout(NO_PROGRESS, pool.get_or_connect(provider)).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(err)) => {
                debug!(provider = %provider.fmt_short(), "no connection for a fetch: {err:#}");
                return Outcome::ConnectFailed;
            }
            Err(_elapsed) => return Outcome::Stalled,
        };
    fetch_over(&connection, &blobs, hash).await
}

/// [`fetch`] over a connection to the provider.
async fn fetch_over(connection: &iroh::endpoint::Connection, blobs: &Store, hash: Hash) -> Outcome {
    let provider = connection.remote_id();
    // Opened here: `fetch` reports a stream that fails to open as a failure of
    // the local store (iroh-blobs-007).
    let pair = match n0_future::time::timeout(NO_PROGRESS, connection.open_bi()).await {
        Ok(Ok((send, recv))) => iroh_blobs::get::StreamPair::new(
            u64::try_from(connection.stable_id()).unwrap_or_default(),
            recv,
            send,
        ),
        Ok(Err(err)) => {
            debug!(provider = %provider.fmt_short(), %hash, "no stream for a fetch: {err:#}");
            return Outcome::Failed;
        }
        Err(_elapsed) => return Outcome::Stalled,
    };
    let progress = blobs
        .remote()
        .fetch(pair, HashAndFormat::raw(hash))
        .stream();
    tokio::pin!(progress);
    loop {
        match n0_future::time::timeout(NO_PROGRESS, progress.next()).await {
            Err(_elapsed) => return Outcome::Stalled,
            Ok(Some(GetProgressItem::Progress(_))) => {}
            Ok(Some(GetProgressItem::Done(_))) => return Outcome::Fetched,
            Ok(Some(GetProgressItem::Error(err))) => {
                debug!(provider = %provider.fmt_short(), %hash, "fetch failed: {err:#}");
                return outcome_of_failure(&err);
            }
            Ok(None) => return Outcome::Failed,
        }
    }
}

/// Whose failure a fetch's error is: the node's own blob store's, the
/// provider's answer that it lacks the payload, or the provider's.
fn outcome_of_failure(err: &GetError) -> Outcome {
    match err {
        GetError::LocalFailure { .. } => Outcome::LocalFailure,
        GetError::AtBlobHeaderNext {
            source: AtBlobHeaderNextError::NotFound { .. },
            ..
        }
        | GetError::Decode {
            source:
                DecodeError::ChunkNotFound { .. }
                | DecodeError::ParentNotFound { .. }
                | DecodeError::LeafNotFound { .. },
            ..
        } => Outcome::Lacks,
        // A stream the provider reset with an error code: it read the request
        // and refused it, its connection standing.
        _ if err.open().is_none() && err.iroh_error_code().is_some() => Outcome::Lacks,
        _ => Outcome::Failed,
    }
}

/// The content of every entry of `namespace` that `blobs` does not hold
/// whole.
async fn missing_content(
    sync: &SyncHandle,
    blobs: &Store,
    namespace: NamespaceId,
) -> Result<Vec<Hash>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    sync.get_many(namespace, crate::store::Query::all().into(), tx)
        .await?;
    let mut missing = Vec::new();
    while let Some(entry) = rx.recv().await {
        let entry = entry?;
        if entry.content_len() == 0 {
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
    use iroh::{
        endpoint::presets,
        protocol::{AcceptError, ProtocolHandler, Router},
    };

    use super::*;

    /// Accepts a connection and closes it at once.
    #[derive(Debug, Clone)]
    struct Closing;

    impl ProtocolHandler for Closing {
        async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
            connection.close(0u32.into(), b"closing");
            Ok(())
        }
    }

    /// A request over a connection its provider closed is the provider's
    /// failure, though iroh-blobs reports a stream that cannot open as a
    /// failure of the local store (iroh-blobs-007).
    #[tokio::test]
    async fn a_request_over_a_connection_the_provider_closed_is_the_providers_failure(
    ) -> anyhow::Result<()> {
        let provider = Router::builder(
            Endpoint::builder(presets::Minimal)
                .bind_addr("127.0.0.1:0")?
                .bind()
                .await?,
        )
        .accept(iroh_blobs::ALPN, Closing)
        .spawn();
        let fetching = Endpoint::builder(presets::Minimal)
            .bind_addr("127.0.0.1:0")?
            .bind()
            .await?;
        let connection = fetching
            .connect(provider.endpoint().addr(), iroh_blobs::ALPN)
            .await?;
        connection.closed().await;
        let store = iroh_blobs::store::mem::MemStore::new();

        let outcome = fetch_over(&connection, &store, Hash::new(b"payload")).await;
        assert_eq!(outcome, Outcome::Failed);

        fetching.close().await;
        provider.shutdown().await?;
        Ok(())
    }

    /// A provider that does not hold the payload answers that it lacks it,
    /// which is no failure of the provider.
    #[tokio::test]
    async fn a_provider_without_the_payload_answers_that_it_lacks_it() -> anyhow::Result<()> {
        let held = iroh_blobs::store::mem::MemStore::new();
        let provider = Router::builder(
            Endpoint::builder(presets::Minimal)
                .bind_addr("127.0.0.1:0")?
                .bind()
                .await?,
        )
        .accept(
            iroh_blobs::ALPN,
            iroh_blobs::BlobsProtocol::new(&held, None),
        )
        .spawn();
        let fetching = Endpoint::builder(presets::Minimal)
            .bind_addr("127.0.0.1:0")?
            .bind()
            .await?;
        let connection = fetching
            .connect(provider.endpoint().addr(), iroh_blobs::ALPN)
            .await?;
        let store = iroh_blobs::store::mem::MemStore::new();

        let outcome = fetch_over(&connection, &store, Hash::new(b"payload")).await;
        assert_eq!(outcome, Outcome::Lacks);

        fetching.close().await;
        provider.shutdown().await?;
        Ok(())
    }

    /// A fetch failing on the node's own blob store is the store's failure,
    /// and one failing on the provider's answer is the provider's.
    #[test]
    fn a_fetch_failing_on_the_local_store_is_no_providers_failure() {
        let local = n0_error::e!(GetError::LocalFailure, n0_error::anyerr!("disk full"));
        assert_eq!(outcome_of_failure(&local), Outcome::LocalFailure);
        let remote = n0_error::e!(GetError::BadRequest, n0_error::anyerr!("refused"));
        assert_eq!(outcome_of_failure(&remote), Outcome::Failed);
    }

    /// An event sent to lossy subscribers whose receivers are all gone
    /// completes rather than waiting on them.
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
