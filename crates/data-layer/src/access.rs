//! Caller classification for reconciliation sessions, decided from
//! material one hosted identity already holds — its own PMS and the
//! membership stores of its pods. Nothing is presented over the wire: the
//! transport-authenticated caller node id, the identities the session names
//! and the requested namespace are the only inputs. One book per hosted
//! identity, so a verdict is never widened by what a co-located identity
//! holds (ADR-0013).

use std::{
    collections::HashSet,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use anyhow::Result;
use pdn_store::{
    api::Doc, store::Query, AuthorId, EntryFilter, Identity, NamespaceId, RestsOn, SessionAccess,
    SessionIngest, SessionRole, ValidateOutcome,
};
use pdn_types::{NodeId, PdnId, PodId};

use crate::{
    pod::{
        departure_past, held_entries, HeldEntry, HeldPayload, Member, MembershipKey,
        MembershipView, PastEntry, PodStore, Rests,
    },
    registry::Registry,
};

/// How long a pod store's session waits for the payloads its caller's own
/// entries lack before it is refused; the payloads are small and follow
/// their entries at once.
const CALLER_PAYLOADS_WAIT: Duration = Duration::from_secs(5);

/// A hosted identity as the store names it on the wire: the 32 bytes of
/// its `PdnId`, which the store compares and never interprets.
pub fn identity_of(identity: PdnId) -> Identity {
    Identity::from_bytes(*identity.as_bytes())
}

/// The write-side half of a classification. It rides the session it was
/// decided for, so a set frozen at setup holds for that session alone.
#[derive(Debug, Clone)]
enum WriteAdmission {
    /// Everything the peer offers — what an own device is admitted.
    Whole,
    /// Exactly these entries of a pod's membership store, by key and
    /// author: a former member's departure's past, what a member device
    /// takes from that member's device.
    Past(Arc<HashSet<PastEntry>>),
    /// What a replica whose own identity departed takes from a member
    /// device, a new join or an event reversing the departure included.
    Rests(Arc<Rests>),
    /// Exactly the entries these authors signed and the device statements
    /// of `statements_of`, on a pod's store: what a dial takes from a callee
    /// it serves nothing, and what an accept takes from a node no statement
    /// of the current member it names lists. A statement carries its
    /// member's signature, which the membership view checks whoever
    /// authored the entry.
    Signed {
        authors: Arc<HashSet<AuthorId>>,
        statements_of: Option<PdnId>,
    },
}

/// A membership store as one classification read it: the membership view and
/// the entries it was built from.
type Read = (MembershipView, Vec<HeldEntry>);

/// What a dial still takes from a pod session's callee it serves nothing.
enum Unserved {
    /// Everything: the membership view counts no current member, or lists
    /// no device of the current member the callee names, as while the
    /// payloads of their statements are on their way.
    Unresolved,
    /// The entries every device of a current member signed, and the device
    /// statements of the current member the callee names: a device none of
    /// them lists yet passes on its own statement.
    Members {
        authors: Arc<HashSet<AuthorId>>,
        statements_of: Option<PdnId>,
    },
    /// Nothing: the pod is not held, or the replica's own identity
    /// departed.
    Nothing,
}

/// The classification material one hosted identity holds, consulted per
/// session by the access provider its engine was assembled with.
#[derive(Debug)]
pub(crate) struct AccessBook {
    /// Whom this book judges for.
    identity: PdnId,
    /// This identity's own PMS, armed by `host_identity`. Until it is here
    /// a session on the PMS is refused and no device counts as a sibling:
    /// the device records are in it.
    pms: RwLock<Option<Doc>>,
    /// Set right after the stack spawns, before any session can arrive.
    blobs: OnceLock<iroh_blobs::api::Store>,
    /// Set once the stack spawns, before the identity is hosted; what an
    /// accept decision rests on is read through it.
    sync: OnceLock<pdn_store::actor::SyncHandle>,
    #[cfg(feature = "test-util")]
    pod_verdicts: OnceLock<PodVerdictSink>,
    #[cfg(feature = "test-util")]
    session_traces: OnceLock<SessionTraceSink>,
    /// Holds this identity's next session in one role on one namespace with
    /// one peer once its access is decided, before the decision returns to
    /// the engine.
    #[cfg(feature = "test-util")]
    session_pause: std::sync::Mutex<Option<SessionPauseSlot>>,
    /// Refuses every pod session this identity is asked to serve, as a
    /// device out of reach answers none.
    #[cfg(feature = "test-util")]
    refuse_pods: std::sync::atomic::AtomicBool,
    /// Serves every pod session this identity is asked to serve whole, as
    /// a modified node judging no caller answers.
    #[cfg(feature = "test-util")]
    serve_pods_whole: std::sync::atomic::AtomicBool,
}

/// Where each membership view's verdicts go once a scenario takes the
/// channel.
#[cfg(feature = "test-util")]
pub(crate) type PodVerdictSink =
    Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::pod::PodVerdicts>>>>;

/// Where each session decision and each entry a session admits go once a
/// scenario takes the channel.
#[cfg(feature = "test-util")]
pub(crate) type SessionTraceSink =
    Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<SessionTrace>>>>;

#[cfg(feature = "test-util")]
type SessionPauseSlot = (NamespaceId, NodeId, SessionRole, Arc<SessionPause>);

/// The hold [`SyncNode::pause_next_dial_for_test`](crate::SyncNode::pause_next_dial_for_test)
/// and [`SyncNode::pause_next_accept_for_test`](crate::SyncNode::pause_next_accept_for_test)
/// put on a session: `reached` once its access is decided.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub struct SessionPause {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

/// One session decision of one identity's book, or one entry that
/// session's ingest admitted; `at` orders the traces of several nodes of
/// one process.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone)]
pub struct SessionTrace {
    pub at: std::time::Instant,
    pub identity: PdnId,
    pub namespace: NamespaceId,
    pub role: SessionRole,
    /// The party across the session, as the book judged it.
    pub remote: Identity,
    pub peer: NodeId,
    pub traced: Traced,
}

#[cfg(feature = "test-util")]
#[derive(Debug, Clone)]
pub enum Traced {
    Denied,
    /// Allowed, this side's egress whole.
    AllowedWhole,
    /// Allowed, this side's egress narrowed — over a past, or closed.
    AllowedFiltered,
    Admitted {
        key: Vec<u8>,
        author: AuthorId,
    },
}

impl AccessBook {
    pub(crate) fn new(identity: PdnId) -> Self {
        Self {
            identity,
            pms: RwLock::new(None),
            blobs: OnceLock::new(),
            sync: OnceLock::new(),
            #[cfg(feature = "test-util")]
            pod_verdicts: OnceLock::new(),
            #[cfg(feature = "test-util")]
            session_traces: OnceLock::new(),
            #[cfg(feature = "test-util")]
            session_pause: std::sync::Mutex::default(),
            #[cfg(feature = "test-util")]
            refuse_pods: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "test-util")]
            serve_pods_whole: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn refuse_pod_sessions(&self, refuse: bool) {
        self.refuse_pods
            .store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn serve_pod_sessions_whole(&self, serve: bool) {
        self.serve_pods_whole
            .store(serve, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn set_blobs(&self, blobs: iroh_blobs::api::Store) {
        let _ = self.blobs.set(blobs);
    }

    pub(crate) fn set_sync(&self, sync: pdn_store::actor::SyncHandle) {
        let _ = self.sync.set(sync);
    }

    pub(crate) fn arm_pms(&self, pms: Doc) -> Result<()> {
        *self
            .pms
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))? = Some(pms);
        Ok(())
    }

    pub(crate) fn armed_pms(&self) -> Result<Option<Doc>> {
        Ok(self
            .pms
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .clone())
    }

    pub(crate) fn disarm_pms(&self) -> Result<()> {
        *self
            .pms
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))? = None;
        Ok(())
    }

    /// Fail-closed everywhere but the PMS, which its ticket bounds
    /// (Invariant 1), and the stores of a pod this identity holds.
    ///
    /// `addressed` is the identity whose replica the session names and
    /// `acting` the identity its caller acts for. Which of the two is the
    /// party across the session follows the role: accepting, it is the
    /// caller; dialing, this node is the caller and the party is the
    /// identity it addressed.
    pub(crate) async fn classify(
        &self,
        registry: Arc<Registry>,
        namespace: NamespaceId,
        addressed: Identity,
        acting: Identity,
        peer: NodeId,
        role: SessionRole,
    ) -> SessionAccess {
        let remote = match role {
            SessionRole::Accept => acting,
            SessionRole::Dial => addressed,
        };
        // A dial decides once its snapshot is open, so only an accept
        // records what its decision reads; every decision below rests on
        // nothing until `resting_on` names it here.
        let read = match role {
            SessionRole::Accept => Some(self.decision_reads(&registry, namespace).await),
            SessionRole::Dial => None,
        };
        let access = match self
            .try_classify(registry, namespace, remote, peer, role)
            .await
        {
            Ok(access) => access,
            Err(_storage_error) => SessionAccess::Deny,
        };
        let access = match read {
            Some(read) => resting_on(access, read),
            None => access,
        };
        #[cfg(feature = "test-util")]
        self.hold_session(namespace, peer, role).await;
        #[cfg(feature = "test-util")]
        let access = self.traced(access, namespace, remote, peer, role);
        access
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn pause_next_session(
        &self,
        namespace: NamespaceId,
        peer: NodeId,
        role: SessionRole,
    ) -> Arc<SessionPause> {
        let pause = Arc::new(SessionPause::default());
        if let Ok(mut slot) = self.session_pause.lock() {
            *slot = Some((namespace, peer, role, Arc::clone(&pause)));
        }
        pause
    }

    #[cfg(feature = "test-util")]
    async fn hold_session(&self, namespace: NamespaceId, peer: NodeId, role: SessionRole) {
        let pause = self.session_pause.lock().ok().and_then(|mut slot| {
            let held = slot.as_ref().is_some_and(|(paused, with, in_role, _)| {
                *paused == namespace && *with == peer && *in_role == role
            });
            held.then(|| slot.take()).flatten()
        });
        if let Some((_namespace, _peer, _role, pause)) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }

    /// The write counts of the replicas a decision on `namespace` reads,
    /// read before the decision reads them.
    async fn decision_reads(&self, registry: &Registry, namespace: NamespaceId) -> RestsOn {
        let (Ok(replicas), Some(sync)) =
            (self.decision_replicas(registry, namespace), self.sync.get())
        else {
            return RestsOn::Unknown;
        };
        let mut read = Vec::with_capacity(replicas.len());
        for replica in replicas {
            let Ok(writes) = sync.writes(replica).await else {
                return RestsOn::Unknown;
            };
            read.push((replica, writes));
        }
        RestsOn::Writes(read)
    }

    /// The PMS and a pod's membership store; nothing for a replica its
    /// ticket alone bounds.
    fn decision_replicas(
        &self,
        registry: &Registry,
        namespace: NamespaceId,
    ) -> Result<Vec<NamespaceId>> {
        let Some((pod, _store)) = registry.pod_of(namespace)? else {
            return Ok(Vec::new());
        };
        let pms = self.own_pms()?.map(|pms| pms.id());
        let membership = registry.pod(pod)?.map(|held| held.membership.id());
        Ok(pms.into_iter().chain(membership).collect())
    }

    /// `access` as decided, reporting the decision and every entry its
    /// ingest admits once a scenario takes the channel.
    #[cfg(feature = "test-util")]
    fn traced(
        &self,
        access: SessionAccess,
        namespace: NamespaceId,
        remote: Identity,
        peer: NodeId,
        role: SessionRole,
    ) -> SessionAccess {
        let sender = self
            .session_traces
            .get()
            .and_then(|sink| sink.lock().ok().and_then(|sender| sender.clone()));
        let Some(sender) = sender else {
            return access;
        };
        let identity = self.identity;
        let report = move |traced: Traced| {
            let _ = sender.send(SessionTrace {
                at: std::time::Instant::now(),
                identity,
                namespace,
                role,
                remote,
                peer,
                traced,
            });
        };
        match access {
            SessionAccess::Deny => {
                report(Traced::Denied);
                SessionAccess::Deny
            }
            SessionAccess::Allow {
                egress,
                ingest,
                rests_on,
            } => {
                report(if egress.is_some() {
                    Traced::AllowedFiltered
                } else {
                    Traced::AllowedWhole
                });
                let judged = ingest;
                let ingest: SessionIngest = Arc::new(move |entry: &pdn_store::SignedEntry| {
                    let outcome = judged(entry);
                    if matches!(outcome, ValidateOutcome::Accept) {
                        report(Traced::Admitted {
                            key: entry.id().key().to_vec(),
                            author: entry.author(),
                        });
                    }
                    outcome
                });
                SessionAccess::Allow {
                    egress,
                    ingest,
                    rests_on,
                }
            }
        }
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn set_session_traces(&self, sink: SessionTraceSink) {
        let _ = self.session_traces.set(sink);
    }

    async fn try_classify(
        &self,
        registry: Arc<Registry>,
        namespace: NamespaceId,
        remote: Identity,
        peer: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        if let Some((pod, store)) = registry.pod_of(namespace)? {
            return self
                .classify_pod(&registry, pod, store, remote, peer, role)
                .await;
        }
        // The PMS is ticket-gated (Invariant 1). Classifying it against
        // its own, possibly not yet converged, device records would
        // deadlock the bootstrap that delivers those records.
        if self.pms_is(namespace)? {
            return Ok(SessionAccess::whole());
        }
        // Held in no role: nothing here can judge the caller.
        Ok(SessionAccess::Deny)
    }

    /// Either of a pod's stores, served whole to a device of the member the
    /// caller names — a sibling by this identity's own PMS, another current
    /// member by the device statements this identity's membership view
    /// counts — and to nobody else, by the pod stores spec, but for the
    /// membership store's exceptions: a former member's device is served its
    /// departure's past, a replica whose own identity departed sends its
    /// own, and a node no statement of a current member lists is taken that
    /// member's statements alone. The record store has none of these
    /// exceptions.
    async fn classify_pod(
        &self,
        registry: &Arc<Registry>,
        pod: PodId,
        store: PodStore,
        remote: Identity,
        peer: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        #[cfg(feature = "test-util")]
        if matches!(role, SessionRole::Accept)
            && self.refuse_pods.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(SessionAccess::Deny);
        }
        #[cfg(feature = "test-util")]
        if matches!(role, SessionRole::Accept)
            && self
                .serve_pods_whole
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Self::whole());
        }
        let Some(held) = registry.pod(pod)? else {
            return Ok(self.refused(role, || Unserved::Nothing));
        };
        let caller = PdnId::from_bytes(*remote.as_bytes());
        if caller == self.identity {
            // A sibling is served the store whole, a tombstone included.
            let peer_key = crate::private_metadata::device_key(&peer);
            if self.peer_is_own_device(peer_key.as_bytes()).await? {
                return Ok(Self::whole());
            }
            if matches!(role, SessionRole::Accept) {
                return Ok(SessionAccess::Deny);
            }
            let read = self
                .membership_view_and_entries(pod, &held.membership)
                .await?;
            return Ok(self.refused(role, || self.unserved(&read, &caller)));
        }
        let read = match self
            .serve_pod(pod, store, &held.membership, caller, peer)
            .await?
        {
            Ok(access) => return Ok(access),
            Err(read) => read,
        };
        // A newcomer's join statement and device statement arrive as
        // payloads after the session that brought their entries, so the
        // session after it would refuse the newcomer for their absence, so
        // the newcomer is judged again once they are here.
        let read = if self.await_payloads_of(&held.membership, &caller).await? {
            match self
                .serve_pod(pod, store, &held.membership, caller, peer)
                .await?
            {
                Ok(access) => return Ok(access),
                Err(read) => read,
            }
        } else {
            read
        };
        if matches!(role, SessionRole::Accept) && self.takes_statements_of(store, &read, &caller) {
            return Ok(SessionAccess::Allow {
                egress: Some(closed_egress()),
                ingest: Self::ingest(WriteAdmission::Signed {
                    authors: Arc::default(),
                    statements_of: Some(caller),
                }),
                rests_on: RestsOn::Nothing,
            });
        }
        Ok(self.refused(role, || self.unserved(&read, &caller)))
    }

    /// Whether a session on `store` from a node no statement of `caller`
    /// lists is still taken `caller`'s device statements: on the membership
    /// store, by a replica whose own identity has not departed, when
    /// `caller` is a current member — so a device whose siblings are gone
    /// reaches a member device with the statement that lists it.
    fn takes_statements_of(
        &self,
        store: PodStore,
        (view, _entries): &Read,
        caller: &PdnId,
    ) -> bool {
        store == PodStore::Membership
            && view
                .member(&self.identity)
                .and_then(Member::departed_at)
                .is_none()
            && view
                .member(caller)
                .is_some_and(|member| member.state.member)
    }

    /// A pod session served nothing (Invariant 2): denied when accepting,
    /// whatever `unserved` would say; when dialing, nothing served but the
    /// replica's own statements while [`Unserved::Unresolved`] holds — the
    /// membership view counts no current member, or lists no device of the
    /// one the callee names — and what it admits taken.
    fn refused(&self, role: SessionRole, unserved: impl FnOnce() -> Unserved) -> SessionAccess {
        if matches!(role, SessionRole::Accept) {
            return SessionAccess::Deny;
        }
        let (admission, egress) = match unserved() {
            // A replica that counts no current member yet sends its own
            // statements, so a device whose siblings are gone reaches a
            // member device — the egress exception Invariant 2 names; it is
            // sent as well when the callee's member lists no device yet.
            Unserved::Unresolved => (WriteAdmission::Whole, self.own_statements()),
            Unserved::Members {
                authors,
                statements_of,
            } => (
                WriteAdmission::Signed {
                    authors,
                    statements_of,
                },
                closed_egress(),
            ),
            Unserved::Nothing => (
                WriteAdmission::Signed {
                    authors: Arc::default(),
                    statements_of: None,
                },
                closed_egress(),
            ),
        };
        SessionAccess::Allow {
            egress: Some(egress),
            ingest: Self::ingest(admission),
            rests_on: RestsOn::Nothing,
        }
    }

    /// This identity's own device statements, and nothing else.
    fn own_statements(&self) -> EntryFilter {
        let own = self.identity;
        Arc::new(move |entry: &pdn_store::SignedEntry| {
            matches!(
                MembershipKey::parse(entry.key()),
                Some(MembershipKey::Devices { member, .. }) if member == own
            )
        })
    }

    /// What a dial takes from `caller` when this replica, having `read` its
    /// membership store, serves it nothing.
    fn unserved(&self, (view, _entries): &Read, caller: &PdnId) -> Unserved {
        if view
            .member(&self.identity)
            .and_then(Member::departed_at)
            .is_some()
        {
            return Unserved::Nothing;
        }
        let current = || view.identities().filter(|(_, member)| member.state.member);
        let named = view.member(caller).filter(|member| member.state.member);
        if current().next().is_none() || named.is_some_and(|member| member.devices.is_empty()) {
            return Unserved::Unresolved;
        }
        let authors = current()
            .flat_map(|(_, member)| member.devices.iter().map(|device| device.author))
            .collect();
        Unserved::Members {
            authors: Arc::new(authors),
            statements_of: named.map(|_| *caller),
        }
    }

    /// How a session naming `caller` from `peer` is served, or what it read
    /// to refuse it: a current member's device whole, and a former member's device on
    /// the membership store over its departure's past alone, both ways; a
    /// replica whose own identity departed sends any member its own
    /// departure's past alone and takes what its [`Rests`] admit.
    async fn serve_pod(
        &self,
        pod: PodId,
        store: PodStore,
        membership: &Doc,
        caller: PdnId,
        peer: NodeId,
    ) -> Result<std::result::Result<SessionAccess, Read>> {
        let (view, entries) = self.membership_view_and_entries(pod, membership).await?;
        // A payload arriving moves the membership view and no write count.
        let unread = entries
            .iter()
            .any(|entry| entry.payload == HeldPayload::Awaited);
        let rested = |access: SessionAccess| {
            if unread {
                resting_on(access, RestsOn::Unknown)
            } else {
                access
            }
        };
        let listed = view
            .member(&caller)
            .is_some_and(|member| member.devices.iter().any(|device| device.node == peer));
        let Some(member) = view.member(&caller).filter(|_| listed) else {
            return Ok(Err((view, entries)));
        };
        let (past, admission) = if member.state.member {
            match departure_past(&view, &entries, &self.identity) {
                None => return Ok(Ok(rested(Self::whole()))),
                Some(own) => {
                    let rests = Rests::of(&entries, &self.identity);
                    (Arc::new(own), WriteAdmission::Rests(Arc::new(rests)))
                }
            }
        } else {
            match departure_past(&view, &entries, &caller) {
                None => return Ok(Err((view, entries))),
                Some(theirs) => {
                    let past = Arc::new(theirs);
                    (Arc::clone(&past), WriteAdmission::Past(past))
                }
            }
        };
        if store == PodStore::Records {
            return Ok(Err((view, entries)));
        }
        Ok(Ok(rested(SessionAccess::Allow {
            egress: Some(Arc::new(move |entry: &pdn_store::SignedEntry| {
                past.contains(&(entry.key().to_vec(), entry.author()))
            })),
            ingest: Self::ingest(admission),
            rests_on: RestsOn::Nothing,
        })))
    }

    fn whole() -> SessionAccess {
        SessionAccess::Allow {
            egress: None,
            ingest: Self::ingest(WriteAdmission::Whole),
            rests_on: RestsOn::Nothing,
        }
    }

    /// Wait, at most [`CALLER_PAYLOADS_WAIT`], for the payloads the entries
    /// of `member`'s own chain and statements still lack; whether the store
    /// holds any such entry, so the caller is judged again even when the
    /// last payload landed before the wait began.
    async fn await_payloads_of(&self, membership: &Doc, member: &PdnId) -> Result<bool> {
        let Some(blobs) = self.blobs.get() else {
            return Ok(false);
        };
        let prefix = format!("member/{member}/");
        let (mut held, mut lacking) = (false, Vec::new());
        let mut entries = std::pin::pin!(membership.get_many(Query::key_prefix(prefix)).await?);
        while let Some(entry) = futures_lite::StreamExt::next(&mut entries).await {
            held = true;
            let hash = entry?.content_hash();
            if !blobs.has(hash).await? {
                lacking.push(hash);
            }
        }
        if lacking.is_empty() {
            return Ok(held);
        }
        let deadline = tokio::time::Instant::now() + CALLER_PAYLOADS_WAIT;
        while tokio::time::Instant::now() < deadline {
            let mut arrived = true;
            for hash in &lacking {
                arrived &= blobs.has(*hash).await?;
            }
            if arrived {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(true)
    }

    /// The membership view of this identity's replica of `pod`'s membership
    /// store, payloads read as far as they have arrived.
    pub(crate) async fn membership_view(
        &self,
        pod: PodId,
        membership: &Doc,
    ) -> Result<MembershipView> {
        Ok(self.membership_view_and_entries(pod, membership).await?.0)
    }

    /// [`membership_view`](Self::membership_view) beside the entries it was
    /// built from.
    pub(crate) async fn membership_view_and_entries(
        &self,
        pod: PodId,
        membership: &Doc,
    ) -> Result<(MembershipView, Vec<HeldEntry>)> {
        let entries = match self.blobs.get() {
            Some(blobs) => held_entries(membership, blobs).await?,
            None => Vec::new(),
        };
        let view = MembershipView::new(&pod, &membership.id(), &entries);
        #[cfg(feature = "test-util")]
        self.report_pod_verdicts(pod, &entries, &view);
        Ok((view, entries))
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn set_pod_verdicts(&self, sink: PodVerdictSink) {
        let _ = self.pod_verdicts.set(sink);
    }

    #[cfg(feature = "test-util")]
    fn report_pod_verdicts(
        &self,
        pod: PodId,
        entries: &[crate::pod::HeldEntry],
        view: &MembershipView,
    ) {
        let Some(sink) = self.pod_verdicts.get() else {
            return;
        };
        let Ok(sender) = sink.lock() else {
            return;
        };
        if let Some(sender) = sender.as_ref() {
            let verdicts = entries
                .iter()
                .zip(view.verdicts())
                .map(|(entry, verdict)| (entry.key.clone(), entry.author, *verdict))
                .collect();
            let _ = sender.send(crate::pod::PodVerdicts {
                identity: self.identity,
                pod,
                verdicts,
            });
        }
    }

    /// The devices the identity's own PMS lists; none before it is
    /// armed.
    pub(crate) async fn own_devices(&self) -> Result<Vec<NodeId>> {
        let Some(pms) = self.own_pms()? else {
            return Ok(Vec::new());
        };
        crate::private_metadata::listed_devices(&pms).await
    }

    /// Whether the peer is a device of this book's own identity.
    async fn peer_is_own_device(&self, peer_key: &[u8]) -> Result<bool> {
        let Some(pms) = self.own_pms()? else {
            return Ok(false);
        };
        device_listed(&pms, peer_key).await
    }

    /// The session's ingest verdict, consulted by the fork inside its
    /// insert path — nothing here may block or read a replica. Whatever
    /// `admission` refuses is dropped in silence and offered again by the
    /// sender in its next session.
    fn ingest(admission: WriteAdmission) -> SessionIngest {
        Arc::new(move |entry: &pdn_store::SignedEntry| {
            let admits = match &admission {
                WriteAdmission::Whole => true,
                WriteAdmission::Past(entries) => {
                    entries.contains(&(entry.id().key().to_vec(), entry.author()))
                }
                WriteAdmission::Rests(rests) => rests.admits(entry.id().key()),
                WriteAdmission::Signed {
                    authors,
                    statements_of,
                } => {
                    let statement = statements_of.is_some_and(|named| {
                        matches!(
                            MembershipKey::parse(entry.id().key()),
                            Some(MembershipKey::Devices { member, .. }) if member == named
                        )
                    });
                    statement || authors.contains(&entry.author())
                }
            };
            if admits {
                ValidateOutcome::Accept
            } else {
                ValidateOutcome::Drop
            }
        })
    }

    fn own_pms(&self) -> Result<Option<Doc>> {
        Ok(self
            .pms
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .clone())
    }

    /// Which ticket-bound role this identity already holds `namespace` in,
    /// named for the refusal that quotes it. The PMS is classified on its
    /// ticket alone (Invariant 1), so a replica that took its role by
    /// mistake is served whole.
    pub(crate) fn ticket_bound_role(&self, namespace: NamespaceId) -> Result<Option<&'static str>> {
        Ok(self.pms_is(namespace)?.then_some("this identity's PMS"))
    }

    fn pms_is(&self, namespace: NamespaceId) -> Result<bool> {
        Ok(self.own_pms()?.is_some_and(|doc| doc.id() == namespace))
    }
}

/// Record-level membership (tombstones excluded). `device_key` is the one
/// shared key definition ([`crate::private_metadata::device_key`]): this test
/// decides "own device", so it must never drift from what the stores write.
async fn device_listed(doc: &Doc, device_key: &[u8]) -> Result<bool> {
    let query = Query::single_latest_per_key().key_exact(device_key);
    Ok(doc.get_one(query).await?.is_some())
}

/// `access` resting on `read`, unless it rests on something no write count
/// shows.
fn resting_on(access: SessionAccess, read: RestsOn) -> SessionAccess {
    match access {
        SessionAccess::Allow {
            egress,
            ingest,
            rests_on,
        } => SessionAccess::Allow {
            egress,
            ingest,
            rests_on: if rests_on == RestsOn::Unknown {
                rests_on
            } else {
                read
            },
        },
        SessionAccess::Deny => SessionAccess::Deny,
    }
}

/// Dial-side stance toward a callee this identity serves nothing: send
/// nothing while still taking what the admission allows.
fn closed_egress() -> EntryFilter {
    Arc::new(|_entry: &pdn_store::SignedEntry| false)
}

pub(crate) fn session_access_provider(
    book: Arc<AccessBook>,
    registry: Arc<Registry>,
) -> pdn_store::SessionAccessProvider {
    Arc::new(move |namespace, addressed, acting, peer, role| {
        let book = Arc::clone(&book);
        let registry = Arc::clone(&registry);
        let peer = NodeId::from_bytes(*peer.as_bytes());
        Box::pin(async move {
            book.classify(registry, namespace, addressed, acting, peer, role)
                .await
        })
    })
}
