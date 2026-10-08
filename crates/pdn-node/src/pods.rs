//! The pods service: creating a pod for a hosted identity, listing its
//! pods and their members, the invite and join dialogue on
//! the pod-join ALPN — the inviter verifies and burns the secret before any
//! state change, names the newcomer's sequence, writes its joined event and
//! device statement, and only then hands over both stores' write tickets —
//! the membership acts, and placing, editing and reading records. The join's
//! refusals are uniform, as pairing's are. The stores underneath are the
//! data layer's pod stores.

use std::{
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use data_layer::{
    devices_verify, join_verifies, pdn_id_of, pod_id_of, pod_inviter_ticket_kind, pod_ticket_kind,
    AcceptError, AddrInfoOptions, AuthorId, Connection, DevicesPayload, DocTicket, EndpointAddr,
    EventKind, JoinedPayload, Member, MemberDevice, MembershipKey, MembershipView, OpId, Operation,
    PodNotice, PodStore, PodTickets, ProtocolHandler, RecordKey, Seq, SyncNode, UnknownEntry,
    UnknownPod, ACT_PAYLOAD,
};
use pdn_types::{NodeId, PdnId, PodId, RecordId, RecordKind, RecordRef};
use rand::{rngs::SysRng, TryRng as _};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, Mutex},
};

use crate::{
    pairing::{
        read_message, write_message, InviterUnreachable, DEFAULT_INVITE_LIFETIME,
        MAX_WIRE_MESSAGE_LEN,
    },
    runtime::{CleanupSupervisor, Runtime, ServingHalves, State},
};

pub(crate) const POD_JOIN_ALPN: &[u8] = b"/pdn/pod-join/0";

/// Any other version is refused before dialing by the joiner, and
/// uniformly by the inviter.
pub const POD_INVITE_FORMAT_VERSION: u8 = 0;

/// A constant because `join` names no budget; without it a hung inviter
/// holds the caller for the transport's idle timeout.
pub const JOIN_DIALOGUE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a leave waits, before its left event, for a session with another
/// member's device on each store: one with a reachable device goes through
/// in about a second, and past it the device is taken as offline.
pub const LEAVE_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// What `join` waits, once both tickets are recorded, for each store's first
/// session; the catch-up a timeout cuts short is the armer's to finish.
pub const JOIN_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(30);

/// The invite payload: bearer-free, since it is shown on a screen. Its
/// string or QR encoding is a host concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodInvite {
    pub version: u8,
    /// Where the joiner dials.
    pub inviter_addr: EndpointAddr,
    pub secret: [u8; 32],
    pub pod: PodId,
}

/// A pod the identity holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PodInfo {
    pub id: PodId,
}

/// A current member of a pod, with its role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PodMember {
    pub id: PdnId,
    pub owner: bool,
}

/// Refused before dialing. Downcast from the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported pod invite version: {version}")]
pub struct UnsupportedPodInviteVersion {
    pub version: u8,
}

/// The dialogue reached the inviter and ended without the tickets.
/// Reasonless by design, as [`crate::EstablishmentRefused`] is. Downcast from
/// the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("join refused by the inviter")]
pub struct JoinRefused;

/// The exchange was still in flight when [`JOIN_DIALOGUE_TIMEOUT`] passed.
/// Downcast from the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("join dialogue did not complete in time")]
pub struct JoinTimeout;

/// Another `join` of the same pod by the same identity is in flight;
/// refused before dialing. Downcast from the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("a join of {pod} by {identity} is already in flight on this runtime")]
pub struct JoinInProgress {
    pub identity: PdnId,
    pub pod: PodId,
}

/// The identity's announcement key pair has not reached this device yet:
/// a device linked a moment ago signs nothing until it does. Downcast from
/// the `anyhow::Error` of `create` and `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the announcement key of {identity} has not reached this device yet")]
pub struct AnnouncementKeyPending {
    pub identity: PdnId,
}

/// A membership act through [`PodsService::act`]. The create act is
/// written by `create`, the invite act inside the join dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodAct {
    Promote(PdnId),
    /// Of another owner.
    Demote(PdnId),
    /// Of another member, an owner or a plain member alike.
    Remove(PdnId),
    Leave,
}

/// Why [`ActRefused`] refused an act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActRefusal {
    /// A promotion, a demotion or a removal by a member that is no owner.
    NotAnOwner,
    /// A demotion or a removal of the acting identity: its way out is leaving.
    OnItself,
    SubjectNotMember,
    /// A demotion of a plain member.
    SubjectNotOwner,
    /// A leave by the pod's one owner while it has other members, until
    /// another member is an owner.
    SoleOwner,
}

/// A membership act the identity's role or the pod's membership does not
/// allow, as the membership view of the identity's replica shows it; refused
/// before anything is written. Downcast from the `anyhow::Error` of `act`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{act:?} refused: {reason:?}")]
pub struct ActRefused {
    pub act: PodAct,
    pub reason: ActRefusal,
}

/// A claim and an immutable-document are placed once: no operation is
/// appended to either, by its member included. Refused before anything is
/// written. Downcast from the `anyhow::Error` of `append_op`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{} {} under {} is placed once", .record.kind, .record.id, .record.member)]
pub struct RecordPlacedOnce {
    pub record: RecordRef,
}

/// No entry of the record reads on the identity's replica: the pod holds
/// none, or none has arrived yet. Downcast from the `anyhow::Error` of
/// `append_op` and `read_ops`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{} {} under {} reads on no entry here", .record.kind, .record.id, .record.member)]
pub struct UnknownRecord {
    pub record: RecordRef,
}

/// `read` takes a claim or an immutable-document, `read_ops` a
/// mergeable-document. Downcast from the `anyhow::Error` of either.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{} {} under {} is not read by this call", .record.kind, .record.id, .record.member)]
pub struct WrongRecordKind {
    pub record: RecordRef,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinRequest {
    version: u8,
    secret: [u8; 32],
    joiner: PdnId,
    announcement_key: [u8; 32],
    /// The highest sequence of the joiner's own chain its replica holds, a
    /// tombstone's included.
    run: u64,
}

/// Sent only after the verify-and-burn: the sequence of the joiner's chain
/// its membership sits at — a fresh one when `write_joined` — and what its
/// next device statement extends.
#[derive(Debug, Serialize, Deserialize)]
struct JoinOffer {
    seq: u64,
    write_joined: bool,
    statement_version: u64,
    devices: Vec<(NodeId, AuthorId)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinAcceptance {
    /// The encoded join statement, signed over the offered sequence;
    /// absent when the offer writes no joined event.
    join_statement: Option<Vec<u8>>,
    /// The encoded device statement at the offered version.
    device_statement: Vec<u8>,
}

/// Sent only after the joined event and the statement are written.
#[derive(Debug, Serialize, Deserialize)]
struct JoinTickets {
    membership: DocTicket,
    records: DocTicket,
}

/// Creating, listing and joining pods on a runtime, and the records in
/// them. `identity` is the hosted identity acting; a call on a pod the
/// identity is no member of fails with [`UnknownPod`].
#[allow(async_fn_in_trait)]
pub trait PodsService {
    /// Derive the pod id, create both stores and write the signed created
    /// event; the identity is the first owner.
    async fn create(&self, identity: PdnId) -> Result<PodId>;

    /// The pods the identity holds, by its PMS.
    async fn list(&self, identity: PdnId) -> Result<Vec<PodInfo>>;

    /// The current members, each with its role.
    async fn members(&self, identity: PdnId, pod: PodId) -> Result<Vec<PodMember>>;

    /// Mint a one-time invite; `lifetime` overrides the short default.
    /// Writes nothing to the pod: the joined event is written once a
    /// newcomer presents the secret.
    async fn invite(
        &self,
        identity: PdnId,
        pod: PodId,
        lifetime: Option<Duration>,
    ) -> Result<PodInvite>;

    /// Join through the invite's dialogue, returning once both stores
    /// caught up and the identity's membership view lists it as a member; the
    /// identity joins as a plain member. A catch-up cut short
    /// fails with [`data_layer::CatchUpTimeout`] and leaves both tickets and
    /// the PMS's entry recorded.
    async fn join(&self, identity: PdnId, invite: PodInvite) -> Result<PodId>;

    /// Write a membership act once the identity's role allows it, both
    /// sequences picked from what the replica holds;
    /// [`ActRefused`] writes nothing. A leave also tombstones the pod in the
    /// identity's PMS at the left event's sequence and forgets the
    /// record store, the membership store kept as the pod's tombstone.
    async fn act(&self, identity: PdnId, pod: PodId, act: PodAct) -> Result<()>;

    /// Place a record under the identity's own name at a fresh id: a
    /// claim's or an immutable-document's one entry, or a
    /// mergeable-document's first operation.
    async fn put_record(
        &self,
        identity: PdnId,
        pod: PodId,
        kind: RecordKind,
        payload: &[u8],
    ) -> Result<RecordRef>;

    /// Append an operation to a mergeable-document, whoever's name it sits
    /// under. [`RecordPlacedOnce`] for a claim or an immutable-document and
    /// [`UnknownRecord`] for a record that reads on no entry here, both
    /// before anything is written.
    async fn append_op(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
        op: &[u8],
    ) -> Result<()>;

    /// A claim's or an immutable-document's payload, the newest where
    /// several entries read; `None` for a record that reads on no entry
    /// here, [`WrongRecordKind`] for a mergeable-document.
    async fn read(&self, identity: PdnId, pod: PodId, record: RecordRef)
        -> Result<Option<Vec<u8>>>;

    /// A mergeable-document's operations that read here, each with its
    /// writer, in the order of their ids and merged into no document state;
    /// [`UnknownRecord`] when none reads, [`WrongRecordKind`] for another
    /// kind.
    async fn read_ops(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
    ) -> Result<Vec<Operation>>;

    /// Every record an entry of which reads here.
    async fn list_records(&self, identity: PdnId, pod: PodId) -> Result<Vec<RecordRef>>;

    /// The entries of both stores outside the key layout, each with its
    /// author.
    async fn list_unknown(&self, identity: PdnId, pod: PodId) -> Result<Vec<UnknownEntry>>;
}

/// The production [`PodsService`].
#[derive(Clone, Copy)]
pub struct RuntimePodsService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimePodsService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }

    /// The node, and the membership view of `identity`'s replica of `pod`,
    /// once `identity` is a member there.
    async fn as_member(
        &self,
        identity: PdnId,
        pod: PodId,
    ) -> Result<(Arc<SyncNode>, MembershipView)> {
        let node = {
            let state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        let membership = node.pod_membership_view(identity, pod).await?;
        require_member(&membership, identity, pod)?;
        Ok((node, membership))
    }
}

impl PodsService for RuntimePodsService<'_> {
    async fn create(&self, identity: PdnId) -> Result<PodId> {
        // Local writes alone: no round trip runs under the lock.
        let state = self.runtime.state.lock().await;
        let hosted = state.hosted(identity)?;
        let keys = hosted
            .pms
            .announcement_key()
            .await?
            .ok_or(AnnouncementKeyPending { identity })?;
        let mut nonce = [0u8; 16];
        SysRng
            .try_fill_bytes(&mut nonce)
            .context("operating-system randomness unavailable")?;
        let creation = keys.creation(nonce);
        let pod = pod_id_of(&identity, &creation.announcement_key, &creation.nonce);
        let node = Arc::clone(&state.node);
        node.create_pod(identity, pod).await?;
        let mut rollback = PodRollback::new(
            Arc::clone(&node),
            identity,
            pod,
            state.cleanup_tasks.clone(),
        );
        let device = MemberDevice {
            node: node.node_id(),
            author: hosted.author,
        };
        let written = async {
            node.write_pod_entry(
                identity,
                pod,
                PodStore::Membership,
                &MembershipKey::created(identity).to_bytes(),
                &creation.encode(),
            )
            .await?;
            let statement = MembershipKey::Devices {
                member: identity,
                version: 1,
            };
            node.write_pod_entry(
                identity,
                pod,
                PodStore::Membership,
                &statement.to_bytes(),
                &keys.device_statement(1, vec![device]).encode(),
            )
            .await?;
            record_pod(&node, &hosted.pms, identity, pod, Seq::FIRST, None).await
        }
        .await;
        match written {
            Ok(()) => {
                rollback.disarm();
                Ok(pod)
            }
            Err(err) => {
                rollback.roll_back().await;
                Err(err)
            }
        }
    }

    async fn list(&self, identity: PdnId) -> Result<Vec<PodInfo>> {
        let state = self.runtime.state.lock().await;
        let mut pods: Vec<PodInfo> = state
            .hosted(identity)?
            .pms
            .held_pods()
            .await?
            .into_iter()
            .map(|id| PodInfo { id })
            .collect();
        pods.sort();
        Ok(pods)
    }

    async fn members(&self, identity: PdnId, pod: PodId) -> Result<Vec<PodMember>> {
        let (_node, membership) = self.as_member(identity, pod).await?;
        let mut members: Vec<PodMember> = membership
            .identities()
            .filter(|(_id, member)| member.state.member)
            .map(|(id, member)| PodMember {
                id: *id,
                owner: member.state.owner,
            })
            .collect();
        members.sort();
        Ok(members)
    }

    async fn invite(
        &self,
        identity: PdnId,
        pod: PodId,
        lifetime: Option<Duration>,
    ) -> Result<PodInvite> {
        self.as_member(identity, pod).await?;
        let mut state = self.runtime.state.lock().await;
        let secret = state.pending_pod_invites.mint(
            (identity, pod),
            lifetime.unwrap_or(DEFAULT_INVITE_LIFETIME),
            Instant::now(),
        )?;
        Ok(PodInvite {
            version: POD_INVITE_FORMAT_VERSION,
            inviter_addr: state.node.dial_handle().addr(),
            secret,
            pod,
        })
    }

    async fn join(&self, identity: PdnId, invite: PodInvite) -> Result<PodId> {
        if invite.version != POD_INVITE_FORMAT_VERSION {
            return Err(UnsupportedPodInviteVersion {
                version: invite.version,
            }
            .into());
        }
        let cleanup_tasks = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            if !state.joining_in_flight.insert((identity, invite.pod)) {
                return Err(JoinInProgress {
                    identity,
                    pod: invite.pod,
                }
                .into());
            }
            state.cleanup_tasks.clone()
        };
        // Released synchronously on every outcome; the reservation's `Drop`
        // covers a cancellation of this future alone.
        let reservation = JoinReservation {
            state: Arc::clone(&self.runtime.state),
            key: (identity, invite.pod),
            armed: true,
            cleanup_tasks,
        };
        let joined = join_via_dialogue(&self.runtime.state, identity, &invite).await;
        reservation.release().await;
        joined
    }

    async fn act(&self, identity: PdnId, pod: PodId, act: PodAct) -> Result<()> {
        if act == PodAct::Leave {
            // Checked before the flush too, so a refused leave dials nobody.
            let node = {
                let state = self.runtime.state.lock().await;
                act_key(&state, identity, pod, act).await?;
                Arc::clone(&state.node)
            };
            // What no member's device holds by the departure never leaves
            // this one: the tombstone serves the departure's past alone.
            // A round trip, so outside the lock.
            let _flushed = node
                .flush_pod(identity, pod)
                .await?
                .wait(LEAVE_FLUSH_TIMEOUT)
                .await;
        }
        // Local writes alone: no round trip runs under the lock.
        let state = self.runtime.state.lock().await;
        let key = act_key(&state, identity, pod, act).await?;
        state
            .node
            .write_pod_entry(
                identity,
                pod,
                PodStore::Membership,
                &key.to_bytes(),
                &ACT_PAYLOAD,
            )
            .await?;
        if let MembershipKey::Event {
            kind: EventKind::Left,
            seq,
            ..
        } = key
        {
            depart(&state, identity, pod, seq).await?;
            // Without it the left event reaches the members at the pod
            // pass's next run, should its announcement have been lost.
            let _dialed = state.node.flush_pod(identity, pod).await;
        }
        Ok(())
    }

    async fn put_record(
        &self,
        identity: PdnId,
        pod: PodId,
        kind: RecordKind,
        payload: &[u8],
    ) -> Result<RecordRef> {
        // Local writes alone: no round trip runs under the lock.
        let state = self.runtime.state.lock().await;
        let (author, mseq) = writer(&state, identity, pod).await?;
        let mut id = [0u8; 16];
        SysRng
            .try_fill_bytes(&mut id)
            .context("operating-system randomness unavailable")?;
        let (member, id) = (identity, RecordId::from_bytes(id));
        let key = match kind {
            RecordKind::Claim => RecordKey::Claim { member, id, mseq },
            RecordKind::ImmutableDocument => RecordKey::ImmutableDocument { member, id, mseq },
            RecordKind::MergeableDocument => RecordKey::Operation {
                member,
                id,
                op: OpId {
                    writer: identity,
                    author,
                    mseq,
                    op_seq: 1,
                },
            },
        };
        state
            .node
            .write_pod_entry(identity, pod, PodStore::Records, &key.to_bytes(), payload)
            .await?;
        Ok(key.record())
    }

    async fn append_op(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
        op: &[u8],
    ) -> Result<()> {
        // Held from the read of the operation sequence to the write that
        // takes it: two appends taking one sequence would share a key, the
        // later replacing the earlier.
        let state = self.runtime.state.lock().await;
        let (author, mseq) = writer(&state, identity, pod).await?;
        if record.kind != RecordKind::MergeableDocument {
            return Err(RecordPlacedOnce { record }.into());
        }
        let view = state
            .node
            .pod_record_view_of(identity, pod, &record)
            .await?;
        // Unread, `record` would be created under another member's name.
        if !view.records().any(|read| *read == record) {
            return Err(UnknownRecord { record }.into());
        }
        let op_seq = view
            .next_op_seq(&record, author)
            .context("operation sequence exhausted")?;
        let key = RecordKey::Operation {
            member: record.member,
            id: record.id,
            op: OpId {
                writer: identity,
                author,
                mseq,
                op_seq,
            },
        };
        state
            .node
            .write_pod_entry(identity, pod, PodStore::Records, &key.to_bytes(), op)
            .await
    }

    async fn read(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
    ) -> Result<Option<Vec<u8>>> {
        let (node, _membership) = self.as_member(identity, pod).await?;
        if record.kind == RecordKind::MergeableDocument {
            return Err(WrongRecordKind { record }.into());
        }
        node.read_pod_record(identity, pod, &record).await
    }

    async fn read_ops(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
    ) -> Result<Vec<Operation>> {
        let (node, _membership) = self.as_member(identity, pod).await?;
        if record.kind != RecordKind::MergeableDocument {
            return Err(WrongRecordKind { record }.into());
        }
        let operations = node.read_pod_operations(identity, pod, &record).await?;
        if operations.is_empty() {
            return Err(UnknownRecord { record }.into());
        }
        Ok(operations)
    }

    async fn list_records(&self, identity: PdnId, pod: PodId) -> Result<Vec<RecordRef>> {
        let (node, _membership) = self.as_member(identity, pod).await?;
        Ok(node
            .pod_record_view(identity, pod)
            .await?
            .records()
            .copied()
            .collect())
    }

    async fn list_unknown(&self, identity: PdnId, pod: PodId) -> Result<Vec<UnknownEntry>> {
        let (node, _membership) = self.as_member(identity, pod).await?;
        node.list_pod_unknown(identity, pod).await
    }
}

/// `identity` as `membership` shows it; [`UnknownPod`] for no member.
fn require_member(membership: &MembershipView, identity: PdnId, pod: PodId) -> Result<&Member> {
    membership
        .member(&identity)
        .filter(|member| member.state.member)
        .ok_or_else(|| UnknownPod { pod }.into())
}

/// The key of the event `act` writes as `identity` in `pod`, at the first
/// sequence of its subject's chain the replica holds no entry at, naming
/// the actor's last; [`ActRefused`] by the checks of
/// [`act_event`].
async fn act_key(state: &State, identity: PdnId, pod: PodId, act: PodAct) -> Result<MembershipKey> {
    state.hosted(identity)?;
    let membership = state.node.pod_membership_view(identity, pod).await?;
    let actor = require_member(&membership, identity, pod)?;
    let (subject, kind) = act_event(&membership, identity, actor, act)
        .map_err(|reason| ActRefused { act, reason })?;
    let seq = membership
        .member(&subject)
        .map_or(0, Member::run)
        .checked_add(1)
        .map(Seq::new)
        .context("membership sequence exhausted")?;
    Ok(MembershipKey::Event {
        subject,
        seq,
        kind,
        actor: identity,
        actor_seq: Seq::new(actor.run()),
    })
}

/// The event `act` writes as `identity`, by the checks the membership view
/// applies to it and the guard on the one owner's leave.
fn act_event(
    membership: &MembershipView,
    identity: PdnId,
    actor: &Member,
    act: PodAct,
) -> Result<(PdnId, EventKind), ActRefusal> {
    let (subject, kind) = match act {
        PodAct::Leave => {
            let current = || {
                membership
                    .identities()
                    .filter(|(_id, member)| member.state.member)
            };
            let owners = current().filter(|(_id, member)| member.state.owner).count();
            if actor.state.owner && owners == 1 && current().count() > 1 {
                return Err(ActRefusal::SoleOwner);
            }
            return Ok((identity, EventKind::Left));
        }
        PodAct::Promote(subject) => (subject, EventKind::Promoted),
        PodAct::Demote(subject) => (subject, EventKind::Demoted),
        PodAct::Remove(subject) => (subject, EventKind::Removed),
    };
    if !actor.state.owner {
        return Err(ActRefusal::NotAnOwner);
    }
    if subject == identity && kind != EventKind::Promoted {
        return Err(ActRefusal::OnItself);
    }
    let target = membership
        .member(&subject)
        .filter(|member| member.state.member)
        .ok_or(ActRefusal::SubjectNotMember)?;
    if kind == EventKind::Demoted && !target.state.owner {
        return Err(ActRefusal::SubjectNotOwner);
    }
    Ok((subject, kind))
}

/// `identity`'s departure from `pod` at `seq` of its chain: the
/// PMS's tombstone at that sequence, then the record store forgotten,
/// the membership store kept as the pod's tombstone.
async fn depart(state: &State, identity: PdnId, pod: PodId, seq: Seq) -> Result<()> {
    state.hosted(identity)?.pms.tombstone_pod(pod, seq).await?;
    state.node.forget_pod(identity, pod).await
}

/// Acts on every notice the data layer reports of a hosted identity's
/// pods: a departure is settled — how a removed member's device, or a
/// departed member's other device, learns of it — and a device its
/// member's statements do not list registers itself. A failure is logged
/// and reported again at the next change to the membership store or the
/// next run of the pod stores' pass.
pub(crate) fn spawn_pod_notice_consumer(
    state: Weak<Mutex<State>>,
    mut notices: mpsc::UnboundedReceiver<PodNotice>,
) {
    let _detached = tokio::spawn(async move {
        while let Some(notice) = notices.recv().await {
            let Some(state) = state.upgrade() else {
                return;
            };
            let guard = state.lock().await;
            match notice {
                PodNotice::Departed { identity, pod, seq } => {
                    if let Err(err) = settle_departure(&guard, identity, pod, seq).await {
                        tracing::warn!(%identity, %pod, "settling the departure failed: {err:#}");
                    }
                }
                PodNotice::Unlisted { identity, pod } => {
                    if let Err(err) = register_device(&guard, identity, pod).await {
                        tracing::warn!(%identity, %pod, "registering this device failed: {err:#}");
                    }
                }
            }
        }
    });
}

async fn settle_departure(state: &State, identity: PdnId, pod: PodId, seq: Seq) -> Result<()> {
    // A join imports onto the tombstone before its joined event arrives.
    if state.joining_in_flight.contains(&(identity, pod)) {
        return Ok(());
    }
    let pms = &state.hosted(identity)?.pms;
    // A later join recorded: the departure ends an earlier membership.
    if pms
        .pod_record(pod)
        .await?
        .is_some_and(|(recorded, held)| held && recorded > seq)
    {
        return Ok(());
    }
    depart(state, identity, pod, seq).await
}

/// Write `identity`'s next device statement in `pod` — its counted list
/// with this device added — when that list does not name this device with
/// the author `identity` writes with here. Nothing while the announcement
/// key has not reached this device, or while a join of the pod is in
/// flight here, its dialogue carrying a statement of its own.
async fn register_device(state: &State, identity: PdnId, pod: PodId) -> Result<()> {
    if state.joining_in_flight.contains(&(identity, pod)) {
        return Ok(());
    }
    let hosted = state.hosted(identity)?;
    let Some(keys) = hosted.pms.announcement_key().await? else {
        return Ok(());
    };
    let membership = state.node.pod_membership_view(identity, pod).await?;
    let member = require_member(&membership, identity, pod)?;
    let device = MemberDevice {
        node: state.node.node_id(),
        author: hosted.author,
    };
    if member.devices.contains(&device) {
        return Ok(());
    }
    let version = member
        .statement_version
        .checked_add(1)
        .context("device statement version exhausted")?;
    let mut devices: Vec<MemberDevice> = member.devices.iter().copied().collect();
    devices.push(device);
    let key = MembershipKey::Devices {
        member: identity,
        version,
    };
    #[cfg(feature = "test-util")]
    if state.failing_device_statements.contains(&pod) {
        anyhow::bail!("the device statement write failed for test");
    }
    state
        .node
        .write_pod_entry(
            identity,
            pod,
            PodStore::Membership,
            &key.to_bytes(),
            &keys.device_statement(version, devices).encode(),
        )
        .await
}

/// Open every pod `identity`'s PMS holds that this device does not,
/// from the tickets beside its record; open the tombstone of every pod it
/// departed that this device holds nothing of, and forget the record store
/// of every one this device still holds. The sweep after a restart
/// re-derives the hosted pods so. A pod whose join is in flight here is
/// the join's; one whose tickets have not arrived waits for the next sweep.
pub(crate) async fn arm_pods(state: &State, identity: PdnId) {
    let Ok(hosted) = state.hosted(identity) else {
        return;
    };
    let (Ok(held), Ok(departed), Ok(holdings)) = (
        hosted.pms.held_pods().await,
        hosted.pms.departed_pods().await,
        state.node.pod_holdings(identity),
    ) else {
        return;
    };
    // `Some(false)` for a tombstone.
    let records_held = |pod: &PodId| {
        holdings
            .iter()
            .find(|(holding, _records)| holding == pod)
            .map(|(_holding, records)| *records)
    };
    let joining = |pod: &PodId| state.joining_in_flight.contains(&(identity, *pod));
    for pod in held {
        if records_held(&pod) == Some(true) || joining(&pod) {
            continue;
        }
        if let Err(err) = open_pod(state, identity, pod).await {
            tracing::warn!(%identity, %pod, "opening the pod from the PMS failed: {err:#}");
        }
    }
    for pod in departed {
        if joining(&pod) {
            continue;
        }
        let armed = match records_held(&pod) {
            Some(true) => state.node.forget_pod(identity, pod).await,
            Some(false) => Ok(()),
            None => open_tombstone(state, identity, pod).await,
        };
        if let Err(err) = armed {
            tracing::warn!(%identity, %pod, "keeping the departed pod's tombstone failed: {err:#}");
        }
    }
}

async fn open_tombstone(state: &State, identity: PdnId, pod: PodId) -> Result<()> {
    let pms = &state.hosted(identity)?.pms;
    let Some(membership) = pms
        .get_ticket(&pod_ticket_kind(&pod, PodStore::Membership))
        .await?
    else {
        return Ok(());
    };
    let inviter = pms
        .get_ticket(&pod_inviter_ticket_kind(&pod, PodStore::Membership))
        .await?;
    state
        .node
        .open_pod_tombstone(identity, pod, membership, inviter.as_slice())
        .await
}

async fn open_pod(state: &State, identity: PdnId, pod: PodId) -> Result<()> {
    let pms = &state.hosted(identity)?.pms;
    let Some(own) = pod_tickets(pms, pod, pod_ticket_kind).await? else {
        return Ok(());
    };
    let inviter = pod_tickets(pms, pod, pod_inviter_ticket_kind).await?;
    let _caught_up = state
        .node
        .import_pod_with(identity, pod, own, inviter.as_slice())
        .await?;
    Ok(())
}

/// Both stores' tickets of `pod` the PMS holds under the kinds
/// `kind` names; `None` until both have arrived.
async fn pod_tickets(
    pms: &data_layer::PrivateMetadataStore,
    pod: PodId,
    kind: fn(&PodId, PodStore) -> String,
) -> Result<Option<PodTickets>> {
    let membership = pms.get_ticket(&kind(&pod, PodStore::Membership)).await?;
    let records = pms.get_ticket(&kind(&pod, PodStore::Records)).await?;
    Ok(membership
        .zip(records)
        .map(|(membership, records)| PodTickets {
            membership,
            records,
        }))
}

/// The author `identity` writes with here and the point of its chain its
/// records name, once it is a member of `pod`.
async fn writer(state: &State, identity: PdnId, pod: PodId) -> Result<(AuthorId, Seq)> {
    let author = state.hosted(identity)?.author;
    let membership = state.node.pod_membership_view(identity, pod).await?;
    let seq = require_member(&membership, identity, pod)?.run();
    Ok((author, Seq::new(seq)))
}

/// The joiner's half: dial, run the dialogue, then record both tickets and
/// the PMS's entry before the catch-up the join waits for.
#[allow(clippy::too_many_lines)] // one dialogue, both transports and each record in one place
async fn join_via_dialogue(
    state: &Arc<Mutex<State>>,
    identity: PdnId,
    invite: &PodInvite,
) -> Result<PodId> {
    let (node, keys, author) = {
        let state = state.lock().await;
        let hosted = state.hosted(identity)?;
        let keys = hosted
            .pms
            .announcement_key()
            .await?
            .ok_or(AnnouncementKeyPending { identity })?;
        (Arc::clone(&state.node), keys, hosted.author)
    };
    let dial = node.dial_handle();
    // An invite carrying this node's own wire identity is a co-located
    // identity's: the dialogue runs over a pipe.
    let connection = if invite.inviter_addr.id == dial.id() {
        None
    } else {
        Some(
            dial.connect(invite.inviter_addr.clone(), POD_JOIN_ALPN)
                .await
                .context(InviterUnreachable)?,
        )
    };
    let request = JoinRequest {
        version: POD_INVITE_FORMAT_VERSION,
        secret: invite.secret,
        joiner: identity,
        announcement_key: keys.public_key(),
        run: node.pod_chain_run(identity, invite.pod, identity).await?,
    };
    let device = MemberDevice {
        node: node.node_id(),
        author,
    };
    let accept = |offer: &JoinOffer| {
        let join_statement = offer.write_joined.then(|| {
            keys.join_statement(&invite.pod, Seq::new(offer.seq))
                .encode()
        });
        let mut devices: Vec<MemberDevice> = offer
            .devices
            .iter()
            .map(|(node, author)| MemberDevice {
                node: *node,
                author: *author,
            })
            .collect();
        if !devices.contains(&device) {
            devices.push(device);
        }
        JoinAcceptance {
            join_statement,
            device_statement: keys
                .device_statement(offer.statement_version, devices)
                .encode(),
        }
    };
    let dialogue = async {
        match &connection {
            Some(connection) => {
                let (mut send, mut recv) = connection.open_bi().await?;
                let joined = run_joiner(&mut send, &mut recv, &request, accept).await;
                send.finish()?;
                joined
            }
            None => join_in_process(state, &request, accept).await,
        }
    };
    let (offer, tickets) = match tokio::time::timeout(JOIN_DIALOGUE_TIMEOUT, dialogue).await {
        Ok(result) => result?,
        Err(_ceiling_passed) => return Err(JoinTimeout.into()),
    };
    if let Some(connection) = &connection {
        connection.close(0u32.into(), b"done");
    }

    let pod = invite.pod;
    let inviter = PodTickets {
        membership: tickets.membership,
        records: tickets.records,
    };
    let caught_up = node.import_pod(identity, pod, inviter.clone()).await?;
    {
        let state = state.lock().await;
        let pms = &state.hosted(identity)?.pms;
        record_pod(
            &node,
            pms,
            identity,
            pod,
            Seq::new(offer.seq),
            Some(&inviter),
        )
        .await?;
    }
    #[cfg(feature = "test-util")]
    {
        let pause = state.lock().await.join_catch_up_pause.take();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }
    let deadline = Instant::now() + JOIN_CATCH_UP_TIMEOUT;
    caught_up.wait(JOIN_CATCH_UP_TIMEOUT).await?;
    // A member from here on: the caller's next act checks the membership view.
    node.await_pod_member(
        identity,
        pod,
        deadline.saturating_duration_since(Instant::now()),
    )
    .await?;
    Ok(pod)
}

/// The sequence the inviter offers the joiner and whether it writes a
/// joined event there, from what the inviter's replica holds of the
/// joiner's chain and the run the joiner reports of its own. A member the
/// inviter knows keeps its point, unless its own chain runs past the
/// inviter's view — a departure the inviter has not yet seen — and then the
/// joined event goes past both runs.
fn offered_seq(joined_at: Option<u64>, known_run: u64, joiner_run: u64) -> (u64, bool) {
    match joined_at {
        Some(joined) if joiner_run <= known_run => (joined, false),
        _ => (known_run.max(joiner_run).saturating_add(1), true),
    }
}

/// The joiner's messages over any pair of streams: the request, the
/// acceptance of the offer the inviter answers it with, and the tickets.
async fn run_joiner<R, W>(
    send: &mut W,
    recv: &mut R,
    request: &JoinRequest,
    accept: impl Fn(&JoinOffer) -> JoinAcceptance,
) -> Result<(JoinOffer, JoinTickets)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_message(send, request).await?;
    // A refusal is just the stream closing.
    let offer: JoinOffer = read_message(recv).await.context(JoinRefused)?;
    write_message(send, &accept(&offer)).await?;
    let tickets: JoinTickets = read_message(recv).await.context(JoinRefused)?;
    Ok((offer, tickets))
}

/// What a pipe between two identities of one node buffers each way: one
/// framed message at [`MAX_WIRE_MESSAGE_LEN`] plus its length prefix.
#[allow(clippy::as_conversions)] // const context, and a 32-bit length fits every usize we build for
const JOIN_PIPE_BYTES: usize = MAX_WIRE_MESSAGE_LEN as usize + 4;

/// The dialogue between two identities of one node, run over a pipe: the
/// same messages and the same verify-and-burn as between two nodes, the
/// serving half entered into the serving halves as `accept` enters it.
async fn join_in_process(
    state: &Arc<Mutex<State>>,
    request: &JoinRequest,
    accept: impl Fn(&JoinOffer) -> JoinAcceptance,
) -> Result<(JoinOffer, JoinTickets)> {
    let serving_halves = state.lock().await.serving_halves.clone();
    let permit = serving_halves.enter().await.ok_or(JoinRefused)?;
    let (dialing, serving) = tokio::io::duplex(JOIN_PIPE_BYTES);
    let (mut dial_recv, mut dial_send) = tokio::io::split(dialing);
    let (mut serve_recv, mut serve_send) = tokio::io::split(serving);
    let served = {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let _permit = permit;
            serve_join(&state, &mut serve_send, &mut serve_recv)
                .await
                .is_some()
        })
    };
    let joined = run_joiner(&mut dial_send, &mut dial_recv, request, accept).await;
    drop((dial_send, dial_recv));
    // The serving half's own outcome decides: a read that failed because it
    // declined must not read as a broken pipe.
    match served.await {
        Ok(true) => joined.context(JoinRefused),
        _ => Err(JoinRefused.into()),
    }
}

/// The inviter's half, over any pair of streams: read the request, verify
/// and burn the secret before any state change, offer the newcomer its
/// sequence, write its joined event and device statement into the replica
/// of the identity the secret was minted for, then hand over both tickets.
/// `None` is a refusal, any reason at all.
#[allow(clippy::too_many_lines)] // one dialogue, each refusal beside the step it guards
pub(crate) async fn serve_join<R, W>(
    state: &Arc<Mutex<State>>,
    send: &mut W,
    recv: &mut R,
) -> Option<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let request: JoinRequest = read_message(recv).await.ok()?;
    if request.version != POD_INVITE_FORMAT_VERSION
        || pdn_id_of(&request.announcement_key) != request.joiner
    {
        return None;
    }
    let (node, identity, pod) = {
        let mut state = state.lock().await;
        // Before any state change.
        let (identity, pod) = state
            .pending_pod_invites
            .verify_and_burn(&request.secret, Instant::now())?;
        (Arc::clone(&state.node), identity, pod)
    };
    let membership = node.pod_membership_view(identity, pod).await.ok()?;
    let inviter = membership.member(&identity)?;
    if !inviter.state.member {
        return None;
    }
    let actor_seq = inviter.run();
    let known = membership.member(&request.joiner);
    let (seq, write_joined) = offered_seq(
        known.and_then(Member::joined_at),
        known.map_or(0, Member::run),
        request.run,
    );
    let known_devices: Vec<MemberDevice> = known
        .map(|member| member.devices.iter().copied().collect())
        .unwrap_or_default();
    let statement_version = known.map_or(0, |member| member.statement_version) + 1;
    let offer = JoinOffer {
        seq,
        write_joined,
        statement_version,
        devices: known_devices
            .iter()
            .map(|device| (device.node, device.author))
            .collect(),
    };
    write_message(send, &offer).await.ok()?;
    let acceptance: JoinAcceptance = read_message(recv).await.ok()?;

    let statement = DevicesPayload::decode(&acceptance.device_statement)?;
    if !devices_verify(&request.announcement_key, statement_version, &statement) {
        return None;
    }
    if write_joined {
        let joined = JoinedPayload::decode(acceptance.join_statement.as_deref()?)?;
        if joined.announcement_key != request.announcement_key
            || !join_verifies(&request.joiner, &pod, Seq::new(seq), &joined)
        {
            return None;
        }
        let key = MembershipKey::Event {
            subject: request.joiner,
            seq: Seq::new(seq),
            kind: EventKind::Joined,
            actor: identity,
            actor_seq: Seq::new(actor_seq),
        };
        node.write_pod_entry(
            identity,
            pod,
            PodStore::Membership,
            &key.to_bytes(),
            &joined.encode(),
        )
        .await
        .ok()?;
    }
    if !statement
        .devices
        .iter()
        .all(|device| known_devices.contains(device))
    {
        let key = MembershipKey::Devices {
            member: request.joiner,
            version: statement_version,
        };
        node.write_pod_entry(
            identity,
            pod,
            PodStore::Membership,
            &key.to_bytes(),
            &statement.encode(),
        )
        .await
        .ok()?;
    }
    #[cfg(feature = "test-util")]
    if std::mem::take(&mut state.lock().await.drop_next_join_reply) {
        return None;
    }
    // Written before the reply: a lost reply leaves the newcomer listed,
    // and a second invite hands the tickets over again.
    let tickets = node
        .share_pod_tickets(identity, pod, AddrInfoOptions::RelayAndAddresses)
        .await
        .ok()?;
    write_message(
        send,
        &JoinTickets {
            membership: tickets.membership,
            records: tickets.records,
        },
    )
    .await
    .ok()?;
    Some(())
}

/// The identity holds `pod` from `seq` of its chain on: both stores' write
/// tickets and the pod's entry in its PMS, which reach
/// its other devices. Beside its own tickets go the ones the inviter handed
/// over at a join, so a sibling, or this device after a restart, has a
/// member's device to dial, named as that member, before its membership view
/// lists anyone.
async fn record_pod(
    node: &data_layer::SyncNode,
    pms: &data_layer::PrivateMetadataStore,
    identity: PdnId,
    pod: PodId,
    seq: Seq,
    inviter: Option<&PodTickets>,
) -> Result<()> {
    let own = node
        .share_pod_tickets(identity, pod, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let mut kinds = vec![
        (pod_ticket_kind(&pod, PodStore::Membership), &own.membership),
        (pod_ticket_kind(&pod, PodStore::Records), &own.records),
    ];
    if let Some(inviter) = inviter {
        kinds.push((
            pod_inviter_ticket_kind(&pod, PodStore::Membership),
            &inviter.membership,
        ));
        kinds.push((
            pod_inviter_ticket_kind(&pod, PodStore::Records),
            &inviter.records,
        ));
    }
    for (kind, ticket) in kinds {
        pms.put_ticket(&kind, ticket).await?;
    }
    pms.record_pod(pod, seq).await
}

/// Filled once, right after the node spawns, and held weakly, as pairing's
/// slot is.
pub(crate) type StateSlot = Arc<OnceLock<Weak<Mutex<State>>>>;

/// The accept side of the join dialogue.
#[derive(Debug, Clone)]
pub(crate) struct JoinHandler {
    state: StateSlot,
    serving_halves: ServingHalves,
}

impl JoinHandler {
    pub(crate) fn new(serving_halves: ServingHalves) -> Self {
        Self {
            state: Arc::default(),
            serving_halves,
        }
    }

    pub(crate) fn slot(&self) -> StateSlot {
        Arc::clone(&self.state)
    }

    async fn serve(&self, connection: &Connection) -> Option<()> {
        let (mut send, mut recv) = connection.accept_bi().await.ok()?;
        let state = self.state.get()?.upgrade()?;
        serve_join(&state, &mut send, &mut recv).await?;
        send.finish().ok()?;
        // Held until the joiner closes, so the tickets are not cut off.
        connection.closed().await;
        Some(())
    }
}

impl ProtocolHandler for JoinHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let served = match self.serving_halves.enter().await {
            Some(_permit) => self.serve(&connection).await,
            None => None,
        };
        if served.is_none() {
            // The one uniform refusal.
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }
}

/// Reserves an `(identity, pod)` pair against a concurrent `join`.
struct JoinReservation {
    state: Arc<Mutex<State>>,
    key: (PdnId, PodId),
    armed: bool,
    cleanup_tasks: CleanupSupervisor,
}

impl JoinReservation {
    async fn release(mut self) {
        self.state.lock().await.joining_in_flight.remove(&self.key);
        self.armed = false;
    }
}

impl Drop for JoinReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = Arc::clone(&self.state);
        let key = self.key;
        self.cleanup_tasks.spawn(async move {
            state.lock().await.joining_in_flight.remove(&key);
        });
    }
}

/// Discards a pod `create` brought up if it fails, or is dropped, before
/// the pod is recorded.
struct PodRollback {
    node: Arc<data_layer::SyncNode>,
    identity: PdnId,
    pod: PodId,
    armed: bool,
    cleanup_tasks: CleanupSupervisor,
}

impl PodRollback {
    fn new(
        node: Arc<data_layer::SyncNode>,
        identity: PdnId,
        pod: PodId,
        cleanup_tasks: CleanupSupervisor,
    ) -> Self {
        Self {
            node,
            identity,
            pod,
            armed: true,
            cleanup_tasks,
        }
    }

    async fn roll_back(&mut self) {
        let _ = self.node.discard_pod(self.identity, self.pod).await;
        self.disarm();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PodRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let node = Arc::clone(&self.node);
        let (identity, pod) = (self.identity, self.pod);
        self.cleanup_tasks.spawn(async move {
            let _ = node.discard_pod(identity, pod).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inviter offers a sequence past a departure only the joiner's own
    /// replica holds yet. Paired: a joiner whose reply was lost, holding
    /// nothing of the pod, keeps the point it joined at, with no second
    /// joined event.
    #[test]
    fn a_departure_the_inviter_has_not_seen_moves_the_offer_past_it() {
        // Joined at 1, promoted at 2 on the inviter; left at 3 on the joiner.
        assert_eq!(offered_seq(Some(1), 2, 3), (4, true));
        assert_eq!(offered_seq(Some(1), 1, 0), (1, false));
        assert_eq!(offered_seq(None, 3, 3), (4, true));
        assert_eq!(offered_seq(None, 0, 0), (1, true));
    }
}
