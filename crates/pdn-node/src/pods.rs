//! The pods service: creating a pod for a hosted identity, listing its
//! pods and their members, the invite and join dialogue on
//! the pod-join ALPN — the inviter verifies and burns the secret before any
//! state change, names the newcomer's sequence and sends the pod's created
//! event, writes the joined event the newcomer signs, and only then hands
//! over both stores' write tickets —
//! the membership acts, and placing, editing and reading records. The join's
//! refusals are uniform, as linking's are. The stores underneath are the
//! data layer's pod stores.

use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use data_layer::{
    creates, devices_verify, join_verifies, pdn_id_of, pod_id_of, pod_inviter_ticket_kind,
    pod_ticket_kind, AcceptError, AddrInfoOptions, AuthorId, Connection, CreatedPayload,
    DevicesPayload, DocTicket, EndpointAddr, EventKind, JoinedPayload, Member, MemberDevice,
    MembershipKey, MembershipView, OpId, Operation, PodNotice, PodStore, PodTickets,
    ProtocolHandler, RecordKey, Seq, SyncNode, UnknownEntry, UnknownPod, ACT_PAYLOAD,
};
use pdn_types::{PdnId, PodId, RecordId, RecordKind, RecordRef};
use rand::{rngs::SysRng, TryRng as _};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, Mutex},
};

use crate::{
    ceremony::{
        read_message, write_message, InviterUnreachable, StateSlot, DEFAULT_INVITE_LIFETIME,
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

/// What a leave waits for a session with another member's device on each
/// store, before its left event and again after it: one with a reachable
/// device goes through in about a second, and past it the device is taken
/// as offline. Payloads that device lacks extend it (`PodFlush::wait`).
pub const LEAVE_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// What `join` waits in all, once both tickets are recorded, for each
/// store's first session and then for its membership view to list the
/// identity; the catch-up a timeout cuts short is the armer's to finish.
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

/// The dialogue reached the inviter and ended without the tickets, or with
/// an offer the joiner refuses: another pod's created event, a sequence past
/// the joiner's own history, tickets to stores the created event does not
/// name. Reasonless by design, as [`crate::LinkingRefused`] is.
/// Downcast from the `anyhow::Error` of `join`.
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

/// The identity key pair has not reached this device yet:
/// a device linked a moment ago signs nothing until it does. Downcast from
/// the `anyhow::Error` of `create` and `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the identity key of {identity} has not reached this device yet")]
pub struct IdentityKeyPending {
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
    identity_key: [u8; 32],
    /// The highest sequence of the joiner's own chain its replica holds or
    /// its PMS records the pod at, a tombstone's included.
    run: u64,
}

/// What a modified inviter sends in a join dialogue in place of the honest
/// value, by [`Runtime::forge_next_join_for_test`].
#[cfg(feature = "test-util")]
#[derive(Debug, Clone, Copy)]
pub enum ForgedJoin {
    /// Offers this sequence, a joined event written there.
    Seq(u64),
    /// Offers the created event of this other pod the inviting identity holds.
    CreationOf(PodId),
    /// Hands over the tickets of this other pod the inviting identity holds.
    TicketsOf(PodId),
}

/// Sent only after the verify-and-burn: the sequence of the joiner's chain
/// its membership sits at — a fresh one when `write_joined` — and the pod's
/// encoded created event, which binds the pod id to its two stores.
#[derive(Debug, Serialize, Deserialize)]
struct JoinOffer {
    seq: u64,
    write_joined: bool,
    creation: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinAcceptance {
    /// The encoded join statement, signed over the offered sequence and the
    /// joining device; absent when the offer writes no joined event.
    join_statement: Option<Vec<u8>>,
    /// When the offer writes no joined event, the joiner's encoded statement
    /// at version 0 naming the joining device alone: a member's device no
    /// statement lists is served by no member device.
    device_statement: Option<Vec<u8>>,
}

/// Sent only after the joined event is written.
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
    /// event; the identity is the first owner. Runs to its end once called:
    /// a dropped call leaves the pod whole and listed, or nothing.
    async fn create(&self, identity: PdnId) -> Result<PodId>;

    /// The pods the identity holds: those whose record store this device
    /// holds, and those its PMS holds that this device has not opened yet.
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
        let state = Arc::clone(&self.runtime.state);
        let cleanup_tasks = state.lock().await.cleanup_tasks.clone();
        // Runs to its end whatever the caller does: cut midway, it would
        // leave stores that no PMS record names.
        cleanup_tasks
            .spawn(async move { create_to_record(&state, identity).await })
            .await
            .context("the pod's create ended without an answer")?
    }

    async fn list(&self, identity: PdnId) -> Result<Vec<PodInfo>> {
        let state = self.runtime.state.lock().await;
        let recorded = state.hosted(identity)?.pms.held_pods().await?;
        let holdings = state.node.pod_holdings(identity)?;
        // What this device holds follows the membership view once it holds
        // a store; the record alone speaks for a pod not opened here yet.
        let unopened = recorded
            .into_iter()
            .filter(|pod| !holdings.iter().any(|(held, _records)| held == pod));
        let opened = holdings
            .iter()
            .filter(|(_pod, records)| *records)
            .map(|(pod, _records)| *pod);
        let mut pods: Vec<PodInfo> = unopened.chain(opened).map(|id| PodInfo { id }).collect();
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
        let mut flushed = false;
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
            flushed = node
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
            let flush = state.node.flush_pod(identity, pod).await;
            drop(state);
            // A device that stops once the leave returns would otherwise
            // count as a member until it comes back. Not waited when the
            // first flush reached no member: offline, it would double the leave.
            if let (true, Ok(flush)) = (flushed, flush) {
                let _flushed = flush.wait(LEAVE_FLUSH_TIMEOUT).await;
            }
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

/// The whole of `create`, its commit point the PMS's record of the pod: a
/// failure before it discards both stores.
async fn create_to_record(state: &Mutex<State>, identity: PdnId) -> Result<PodId> {
    // Local writes alone: no round trip runs under the lock.
    let state = state.lock().await;
    let hosted = state.hosted(identity)?;
    let keys = hosted
        .pms
        .identity_key()
        .await?
        .ok_or(IdentityKeyPending { identity })?;
    let mut nonce = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut nonce)
        .context("operating-system randomness unavailable")?;
    let pod = pod_id_of(&identity, &keys.public_key(), &nonce);
    let node = Arc::clone(&state.node);
    node.create_pod(identity, pod).await?;
    let written = async {
        let (membership_store, record_store) = node.pod_stores(identity, pod)?;
        let creation = keys.creation(nonce, membership_store, record_store);
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
        let device = MemberDevice {
            node: node.node_id(),
            author: hosted.author,
        };
        node.write_pod_entry(
            identity,
            pod,
            PodStore::Membership,
            &statement.to_bytes(),
            &keys.device_statement(&pod, 1, vec![device]).encode(),
        )
        .await?;
        let own = node
            .share_pod_tickets(identity, pod, AddrInfoOptions::RelayAndAddresses)
            .await?;
        record_pod(&node, identity, &hosted.pms, pod, Seq::FIRST, &own, None).await
    }
    .await;
    let Err(err) = written else {
        return Ok(pod);
    };
    match hosted.pms.pod_record(pod).await {
        // The record landed though its write answered with an error.
        Ok(Some(_recorded)) => Ok(pod),
        Ok(None) => {
            if let Err(discard_err) = node.discard_pod(identity, pod).await {
                tracing::warn!(%identity, %pod, "a pod whose create failed stayed held: {discard_err:#}");
            }
            Err(err)
        }
        // Discarded once recorded, the pod would stay listed with empty stores.
        Err(_unread) => Err(err),
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
/// pods: a departure or a reinstatement is settled — how a removed
/// member's device, a departed member's other device, or a device whose
/// member's departure a later event reversed learns of it — and a device
/// its member's statements do not list registers itself. A failure is
/// logged and reported again at the next change to the membership store or
/// the next run of the pod stores' pass.
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
                PodNotice::Departed { identity, pod, .. }
                | PodNotice::Reinstated { identity, pod } => {
                    if let Err(err) = settle_pod(&guard, identity, pod).await {
                        tracing::warn!(%identity, %pod, "settling the pod's holding failed: {err:#}");
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

/// Bring what this device holds of `pod` for `identity` — both stores, the
/// tombstone or nothing — in line with [`holds`]: the record store opened
/// from the PMS's tickets or forgotten, the tombstone opened, and a
/// departure the membership view shows tombstoned in the PMS. Nothing while
/// a join of the pod is in flight here, since a join imports onto the
/// tombstone before its joined event arrives.
async fn settle_pod(state: &State, identity: PdnId, pod: PodId) -> Result<()> {
    if state.joining_in_flight.contains(&(identity, pod)) {
        return Ok(());
    }
    let record = state.hosted(identity)?.pms.pod_record(pod).await?;
    let holding = state.node.pod_holding(identity, pod).await?;
    let own = holding
        .as_ref()
        .and_then(|holding| holding.membership.member(&identity));
    match (
        holds(record, own),
        holding.as_ref().map(|holding| holding.records),
    ) {
        (true, Some(true)) | (false, Some(false)) => Ok(()),
        (true, _) => open_pod(state, identity, pod).await,
        (false, None) => open_tombstone(state, identity, pod).await,
        (false, Some(true)) => match own.and_then(Member::departed_at) {
            Some(seq) => depart(state, identity, pod, Seq::new(seq)).await,
            None => state.node.forget_pod(identity, pod).await,
        },
    }
}

/// Whether `identity` holds a pod, by the highest record its PMS holds of
/// it and by `own`, the identity as the membership view of its replica
/// here shows it: the record decides while the replica holds the
/// identity's chain short of the record's sequence — the event it mirrors
/// has not arrived — and the membership view from there on, so a departure
/// a later event reversed, or a join that counts for nothing, ends where the
/// membership view does.
fn holds(record: Option<(Seq, bool)>, own: Option<&Member>) -> bool {
    match (record, own) {
        (Some((seq, held)), Some(own)) if own.run() < seq.get() => held,
        (_, Some(own)) => own.state.member,
        (Some((_seq, held)), None) => held,
        (None, None) => false,
    }
}

/// Write `identity`'s next device statement in `pod` — its counted list
/// with this device added — when that list does not name this device with
/// the author `identity` writes with here, and reconcile it with the other
/// members' devices at once; a replica that lists nobody yet writes version
/// 1 naming this device alone, which counts beside every other statement,
/// and reaches the devices its tickets name. Nothing while the identity key
/// has not reached this device, or while a join of the pod is in
/// flight here, its join statement naming this device.
async fn register_device(state: &State, identity: PdnId, pod: PodId) -> Result<()> {
    if state.joining_in_flight.contains(&(identity, pod)) {
        return Ok(());
    }
    let hosted = state.hosted(identity)?;
    let Some(keys) = hosted.pms.identity_key().await? else {
        return Ok(());
    };
    let membership = state.node.pod_membership_view(identity, pod).await?;
    let device = MemberDevice {
        node: state.node.node_id(),
        author: hosted.author,
    };
    let (version, mut devices) = if membership.identities().next().is_none() {
        (1, Vec::new())
    } else {
        let member = require_member(&membership, identity, pod)?;
        if member.devices.contains(&device) {
            return Ok(());
        }
        let version = member
            .statement_version
            .checked_add(1)
            .context("device statement version exhausted")?;
        (version, member.devices.iter().copied().collect())
    };
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
            &keys.device_statement(&pod, version, devices).encode(),
        )
        .await?;
    // Its announcement reaches only the swarm's neighbors of the moment.
    let _dialed = state.node.flush_pod(identity, pod).await?;
    Ok(())
}

/// Settle every pod `identity`'s PMS records whose record this device's
/// holding does not match ([`settle_pod`]): a pod held by its record opened
/// from the tickets beside it, a departed one's tombstone opened, or its
/// record store forgotten unless the membership view here shows the
/// departure reversed. The sweep after a restart re-derives the hosted pods
/// so. A pod whose join is in flight here is the join's; one whose tickets
/// have not arrived waits for the next sweep.
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
    let recorded = held
        .into_iter()
        .map(|pod| (pod, true))
        .chain(departed.into_iter().map(|pod| (pod, false)));
    for (pod, by_record) in recorded {
        if records_held(&pod) == Some(by_record)
            || state.joining_in_flight.contains(&(identity, pod))
        {
            continue;
        }
        if let Err(err) = settle_pod(state, identity, pod).await {
            tracing::warn!(%identity, %pod, "settling the pod from the PMS failed: {err:#}");
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

/// The joiner's half: dial, run the dialogue, then record the tickets and
/// the PMS's entry before it imports the stores and waits for their catch-up.
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
            .identity_key()
            .await?
            .ok_or(IdentityKeyPending { identity })?;
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
    // The PMS's record too: a departure can reach this device through the
    // PMS alone, its left event still on the device that wrote it.
    let run = node
        .pod_chain_run(identity, invite.pod, identity)
        .await?
        .max(recorded_seq(state, identity, invite.pod).await?);
    let request = JoinRequest {
        version: POD_INVITE_FORMAT_VERSION,
        secret: invite.secret,
        joiner: identity,
        identity_key: keys.public_key(),
        run,
    };
    // No sequence past the joiner's own history: a statement over a later
    // one would readmit the joiner at a leave it has yet to write.
    let furthest = run.saturating_add(1);
    let device = MemberDevice {
        node: node.node_id(),
        author,
    };
    let accept = |offer: &JoinOffer| {
        let creation = CreatedPayload::decode(&offer.creation)?;
        if !creates(&invite.pod, &creation) || offer.seq == 0 || offer.seq > furthest {
            return None;
        }
        Some(if offer.write_joined {
            JoinAcceptance {
                join_statement: Some(
                    keys.join_statement(&invite.pod, Seq::new(offer.seq), device)
                        .encode(),
                ),
                device_statement: None,
            }
        } else {
            JoinAcceptance {
                join_statement: None,
                device_statement: Some(
                    keys.device_statement(&invite.pod, 0, vec![device]).encode(),
                ),
            }
        })
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
    // The stores the created event names, and no others, are this pod's.
    let creation = CreatedPayload::decode(&offer.creation).ok_or(JoinRefused)?;
    if inviter.membership.capability.id() != creation.membership_store
        || inviter.records.capability.id() != creation.record_store
    {
        return Err(JoinRefused.into());
    }
    // Recorded, tickets the import refuses would leave the pod listed and
    // never opened.
    node.check_pod_import(identity, pod, &inviter)
        .context(JoinRefused)?;
    // The commit point, before the first local effect: from here on the
    // armer's sweep finishes whatever cuts the join short.
    let own = node.own_pod_tickets(identity, &inviter)?;
    {
        let state = state.lock().await;
        let pms = &state.hosted(identity)?.pms;
        record_pod(
            &node,
            identity,
            pms,
            pod,
            Seq::new(offer.seq),
            &own,
            Some(&inviter),
        )
        .await?;
    }
    #[cfg(feature = "test-util")]
    {
        let pause = state.lock().await.join_import_pause.take();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }
    let caught_up = node.import_pod(identity, pod, inviter).await?;
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

/// The highest sequence `identity`'s PMS records `pod` at, a tombstone's
/// included; `0` for a pod it does not record.
async fn recorded_seq(state: &Arc<Mutex<State>>, identity: PdnId, pod: PodId) -> Result<u64> {
    let state = state.lock().await;
    let record = state.hosted(identity)?.pms.pod_record(pod).await?;
    Ok(record.map_or(0, |(seq, _held)| seq.get()))
}

/// The joiner's messages over any pair of streams: the request, the
/// acceptance of the offer the inviter answers it with, and the tickets.
/// An offer `accept` refuses closes the dialogue.
async fn run_joiner<R, W>(
    send: &mut W,
    recv: &mut R,
    request: &JoinRequest,
    accept: impl Fn(&JoinOffer) -> Option<JoinAcceptance>,
) -> Result<(JoinOffer, JoinTickets)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_message(send, request).await?;
    // A refusal is just the stream closing.
    let offer: JoinOffer = read_message(recv).await.context(JoinRefused)?;
    let acceptance = accept(&offer).ok_or(JoinRefused)?;
    write_message(send, &acceptance).await?;
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
    accept: impl Fn(&JoinOffer) -> Option<JoinAcceptance>,
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
            serve_join(&state, &mut serve_send, &mut serve_recv).await
        })
    };
    let joined = run_joiner(&mut dial_send, &mut dial_recv, request, accept).await;
    drop((dial_send, dial_recv));
    // The serving half's own outcome decides: a read that failed because it
    // declined must not read as a broken pipe. Its storage failure is this
    // node's own, so nothing hides it from the caller.
    match served.await {
        Ok(Ok(())) => joined.context(JoinRefused),
        Ok(Err(JoinUnserved::Failed(err))) => Err(err),
        Ok(Err(JoinUnserved::Refused)) | Err(_) => Err(JoinRefused.into()),
    }
}

/// Why the inviter's half of a join handed over no tickets.
#[derive(Debug)]
pub(crate) enum JoinUnserved {
    /// By the dialogue's rules, or over a transport that failed: the one
    /// uniform refusal.
    Refused,
    /// This device's storage, once the secret burned; logged where it
    /// failed, and a remote newcomer is refused all the same.
    Failed(anyhow::Error),
}

impl JoinUnserved {
    /// `err`, met at `step` once the secret burned: a refusal when the pod
    /// is no longer held here, a logged failure otherwise.
    fn after_burn(identity: PdnId, pod: PodId, step: &str, err: anyhow::Error) -> Self {
        if err.downcast_ref::<UnknownPod>().is_some() {
            return Self::Refused;
        }
        tracing::error!(%identity, %pod, "join failed after invite burn while {step}: {err:#}");
        Self::Failed(err)
    }
}

/// The inviter's half, over any pair of streams: read the request, verify
/// and burn the secret before any state change, offer the newcomer its
/// sequence and the pod's created event, write its joined event — or, for
/// a member already, the statement listing a device no statement lists yet
/// — into the replica of the identity the secret was minted for, then hand
/// over both tickets.
#[allow(clippy::too_many_lines)] // one dialogue, each refusal beside the step it guards
pub(crate) async fn serve_join<R, W>(
    state: &Arc<Mutex<State>>,
    send: &mut W,
    recv: &mut R,
) -> Result<(), JoinUnserved>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use JoinUnserved::Refused;
    let request: JoinRequest = read_message(recv).await.map_err(|_| Refused)?;
    if request.version != POD_INVITE_FORMAT_VERSION
        || pdn_id_of(&request.identity_key) != request.joiner
    {
        return Err(Refused);
    }
    #[cfg(feature = "test-util")]
    {
        let pause = state.lock().await.join_serve_pause.take();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }
    let (node, identity, pod) = {
        let mut state = state.lock().await;
        // Before any state change.
        let (identity, pod) = state
            .pending_pod_invites
            .verify_and_burn(&request.secret, Instant::now())
            .ok_or(Refused)?;
        (Arc::clone(&state.node), identity, pod)
    };
    let failed = |step: &'static str| move |err| JoinUnserved::after_burn(identity, pod, step, err);
    #[cfg(feature = "test-util")]
    let forged = state.lock().await.forged_join.take();
    let membership = node
        .pod_membership_view(identity, pod)
        .await
        .map_err(failed("reading the membership"))?;
    let inviter = membership.member(&identity).ok_or(Refused)?;
    if !inviter.state.member {
        return Err(Refused);
    }
    // The PMS lists a device linked a moment ago, which the pod's
    // statements may not name yet.
    let mut inviter_devices: Vec<pdn_types::NodeId> = {
        let state = state.lock().await;
        state
            .hosted(identity)
            .map_err(|_unhosted| Refused)?
            .pms
            .list_devices()
            .await
            .map_err(failed("listing the inviter's devices"))?
    };
    inviter_devices.extend(inviter.devices.iter().map(|device| device.node));
    let actor_seq = inviter.run();
    let known = membership.member(&request.joiner);
    let (seq, write_joined) = offered_seq(
        known.and_then(Member::joined_at),
        known.map_or(0, Member::run),
        request.run,
    );
    let creation = node
        .pod_creation(identity, pod)
        .await
        .map_err(failed("reading the created event"))?
        .ok_or(Refused)?;
    #[cfg(feature = "test-util")]
    let (seq, write_joined, creation) = match forged {
        Some(ForgedJoin::Seq(forged)) => (forged, true, creation),
        Some(ForgedJoin::CreationOf(other)) => (
            seq,
            write_joined,
            node.pod_creation(identity, other)
                .await
                .ok()
                .flatten()
                .ok_or(Refused)?,
        ),
        _ => (seq, write_joined, creation),
    };
    let offer = JoinOffer {
        seq,
        write_joined,
        creation: creation.encode(),
    };
    write_message(send, &offer).await.map_err(|_| Refused)?;
    let acceptance: JoinAcceptance = read_message(recv).await.map_err(|_| Refused)?;

    if write_joined {
        let joined = acceptance
            .join_statement
            .as_deref()
            .and_then(JoinedPayload::decode)
            .ok_or(Refused)?;
        if joined.identity_key != request.identity_key
            || !join_verifies(&request.joiner, &pod, Seq::new(seq), &joined)
        {
            return Err(Refused);
        }
        let key = MembershipKey::Event {
            subject: request.joiner,
            seq: Seq::new(seq),
            kind: EventKind::Joined,
            actor: identity,
            actor_seq: Seq::new(actor_seq),
        };
        #[cfg(feature = "test-util")]
        if std::mem::take(&mut state.lock().await.fail_next_joined_write) {
            let err = anyhow::anyhow!("the joined event's write failed for test");
            return Err(failed("writing the joined event")(err));
        }
        node.write_pod_entry(
            identity,
            pod,
            PodStore::Membership,
            &key.to_bytes(),
            &joined.encode(),
        )
        .await
        .map_err(failed("writing the joined event"))?;
    } else {
        let statement = acceptance
            .device_statement
            .as_deref()
            .and_then(DevicesPayload::decode)
            .ok_or(Refused)?;
        let [device] = statement.devices.as_slice() else {
            return Err(Refused);
        };
        if !devices_verify(&request.identity_key, &pod, &request.joiner, 0, &statement) {
            return Err(Refused);
        }
        if !known.is_some_and(|member| member.devices.contains(device)) {
            let key = MembershipKey::Devices {
                member: request.joiner,
                version: 0,
            };
            node.write_pod_entry(
                identity,
                pod,
                PodStore::Membership,
                &key.to_bytes(),
                &statement.encode(),
            )
            .await
            .map_err(failed("writing the device statement"))?;
        }
    }
    #[cfg(feature = "test-util")]
    if std::mem::take(&mut state.lock().await.drop_next_join_reply) {
        return Err(Refused);
    }
    // Written before the reply: a lost reply leaves the newcomer listed,
    // and a second invite hands the tickets over again.
    #[cfg(feature = "test-util")]
    let pod = match forged {
        Some(ForgedJoin::TicketsOf(other)) => other,
        _ => pod,
    };
    let mut tickets = node
        .share_pod_tickets(identity, pod, AddrInfoOptions::RelayAndAddresses)
        .await
        .map_err(failed("minting the tickets"))?;
    // The inviter's other devices this node knows how to reach: without an
    // address lookup, a joiner whose inviting device goes away knows the
    // others by node id alone and could never reach them.
    let siblings = node.known_addrs(&inviter_devices).await;
    for ticket in [&mut tickets.membership, &mut tickets.records] {
        ticket.nodes.extend(siblings.iter().cloned());
    }
    write_message(
        send,
        &JoinTickets {
            membership: tickets.membership,
            records: tickets.records,
        },
    )
    .await
    .map_err(|_| Refused)
}

/// The identity holds `pod` from `seq` of its chain on: both stores' write
/// tickets and the pod's entry in its PMS, which reach
/// its other devices. Beside its own tickets go the ones the inviter handed
/// over at a join, so a sibling, or this device after a restart, has a
/// member's device to dial, named as that member, before its membership view
/// lists anyone. The entry is written once the tickets are on disk, their
/// payloads included: an entry without them lists a pod no restart opens.
async fn record_pod(
    node: &SyncNode,
    identity: PdnId,
    pms: &data_layer::PrivateMetadataStore,
    pod: PodId,
    seq: Seq,
    own: &PodTickets,
    inviter: Option<&PodTickets>,
) -> Result<()> {
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
    node.flush_stores(identity).await?;
    pms.record_pod(pod, seq).await
}

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
        serve_join(&state, &mut send, &mut recv).await.ok()?;
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
