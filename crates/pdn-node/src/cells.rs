//! The cells service: creating a cell for a hosted identity, listing its
//! cells and their members, and the invite and join dialogue (cells D26) on
//! the cell-join ALPN — the inviter verifies and burns the secret before any
//! state change, names the newcomer's sequence, writes its joined event and
//! device statement, and only then hands over both stores' write tickets.
//! Refusals are uniform, as pairing's are. The stores underneath are the
//! data layer's cell stores.

use std::{
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use data_layer::{
    cell_id_of, cell_ticket_kind, devices_verify, join_verifies, pdn_id_of, AcceptError,
    AddrInfoOptions, AuthorId, CellStore, CellTickets, Connection, DevicesPayload, DocTicket,
    EndpointAddr, EventKind, JoinedPayload, MemberDevice, MembershipKey, ProtocolHandler, Seq,
    UnknownCell,
};
use pdn_types::{CellId, NodeId, PdnId};
use rand::{rngs::SysRng, TryRng as _};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
};

use crate::{
    pairing::{
        read_message, write_message, InviterUnreachable, DEFAULT_INVITE_LIFETIME,
        MAX_WIRE_MESSAGE_LEN,
    },
    runtime::{CleanupSupervisor, Runtime, ServingHalves, State},
};

pub(crate) const CELL_JOIN_ALPN: &[u8] = b"/pdn/cell-join/0";

/// Any other version is refused before dialing by the joiner, and
/// uniformly by the inviter.
pub const CELL_INVITE_FORMAT_VERSION: u8 = 0;

/// A constant because `join` names no budget; without it a hung inviter
/// holds the caller for the transport's idle timeout.
pub const JOIN_DIALOGUE_TIMEOUT: Duration = Duration::from_secs(15);

/// What `join` waits, once both tickets are recorded, for each store's first
/// session; the catch-up a timeout cuts short is the armer's to finish.
pub const JOIN_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(30);

/// The invite payload: bearer-free, since it is shown on a screen. Its
/// string or QR encoding is a host concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellInvite {
    pub version: u8,
    /// Where the joiner dials.
    pub inviter_addr: EndpointAddr,
    pub secret: [u8; 32],
    pub cell: CellId,
}

/// A cell the identity holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellInfo {
    pub id: CellId,
}

/// A current member of a cell, with its role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellMember {
    pub id: PdnId,
    pub owner: bool,
}

/// Refused before dialing. Downcast from the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported cell invite version: {version}")]
pub struct UnsupportedCellInviteVersion {
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

/// Another `join` of the same cell by the same identity is in flight;
/// refused before dialing. Downcast from the `anyhow::Error` of `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("a join of {cell} by {identity} is already in flight on this runtime")]
pub struct JoinInProgress {
    pub identity: PdnId,
    pub cell: CellId,
}

/// The identity's announcement key pair has not reached this device yet:
/// a device linked a moment ago signs nothing until it does. Downcast from
/// the `anyhow::Error` of `create` and `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the announcement key of {identity} has not reached this device yet")]
pub struct AnnouncementKeyPending {
    pub identity: PdnId,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinRequest {
    version: u8,
    secret: [u8; 32],
    joiner: PdnId,
    announcement_key: [u8; 32],
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

/// Creating, listing and joining cells on a runtime. `identity` is the
/// hosted identity acting; a call on a cell the identity is no member of
/// fails with [`UnknownCell`].
#[allow(async_fn_in_trait)]
pub trait CellsService {
    /// Derive the cell id, create both stores and write the signed founding
    /// event; the identity is the first owner.
    async fn create(&self, identity: PdnId) -> Result<CellId>;

    /// The cells the identity holds, by its directory.
    async fn list(&self, identity: PdnId) -> Result<Vec<CellInfo>>;

    /// The current members, each with its role.
    async fn members(&self, identity: PdnId, cell: CellId) -> Result<Vec<CellMember>>;

    /// Mint a one-time invite; `lifetime` overrides the short default.
    /// Writes nothing to the cell: the joined event is written once a
    /// newcomer presents the secret.
    async fn invite(
        &self,
        identity: PdnId,
        cell: CellId,
        lifetime: Option<Duration>,
    ) -> Result<CellInvite>;

    /// Join through the invite's dialogue, returning once both stores
    /// caught up; the identity joins as a plain member. A catch-up cut short
    /// fails with [`data_layer::CatchUpTimeout`] and leaves both tickets and
    /// the directory's entry recorded.
    async fn join(&self, identity: PdnId, invite: CellInvite) -> Result<CellId>;
}

/// The production [`CellsService`].
#[derive(Clone, Copy)]
pub struct RuntimeCellsService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeCellsService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }
}

impl CellsService for RuntimeCellsService<'_> {
    async fn create(&self, identity: PdnId) -> Result<CellId> {
        // Local writes alone: no round trip runs under the lock.
        let state = self.runtime.state.lock().await;
        let hosted = state.hosted(identity)?;
        let keys = hosted
            .directory
            .announcement_key()
            .await?
            .ok_or(AnnouncementKeyPending { identity })?;
        let mut nonce = [0u8; 16];
        SysRng
            .try_fill_bytes(&mut nonce)
            .context("operating-system randomness unavailable")?;
        let founding = keys.founding(nonce);
        let cell = cell_id_of(&identity, &founding.announcement_key, &founding.nonce);
        let node = Arc::clone(&state.node);
        node.create_cell(identity, cell).await?;
        let mut rollback = CellRollback::new(
            Arc::clone(&node),
            identity,
            cell,
            state.cleanup_tasks.clone(),
        );
        let device = MemberDevice {
            node: node.node_id(),
            author: hosted.author,
        };
        let written = async {
            node.write_cell_entry(
                identity,
                cell,
                CellStore::Membership,
                &MembershipKey::founded(identity).to_bytes(),
                &founding.encode(),
            )
            .await?;
            let statement = MembershipKey::Devices {
                member: identity,
                version: 1,
            };
            node.write_cell_entry(
                identity,
                cell,
                CellStore::Membership,
                &statement.to_bytes(),
                &keys.device_statement(1, vec![device]).encode(),
            )
            .await?;
            record_cell(&node, &hosted.directory, identity, cell, Seq::FIRST).await
        }
        .await;
        match written {
            Ok(()) => {
                rollback.disarm();
                Ok(cell)
            }
            Err(err) => {
                rollback.roll_back().await;
                Err(err)
            }
        }
    }

    async fn list(&self, identity: PdnId) -> Result<Vec<CellInfo>> {
        let state = self.runtime.state.lock().await;
        let mut cells: Vec<CellInfo> = state
            .hosted(identity)?
            .directory
            .held_cells()
            .await?
            .into_iter()
            .map(|id| CellInfo { id })
            .collect();
        cells.sort();
        Ok(cells)
    }

    async fn members(&self, identity: PdnId, cell: CellId) -> Result<Vec<CellMember>> {
        let node = {
            let state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        let membership = node.cell_membership(identity, cell).await?;
        if !membership
            .member(&identity)
            .is_some_and(|member| member.state.member)
        {
            return Err(UnknownCell { cell }.into());
        }
        let mut members: Vec<CellMember> = membership
            .identities()
            .filter(|(_id, member)| member.state.member)
            .map(|(id, member)| CellMember {
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
        cell: CellId,
        lifetime: Option<Duration>,
    ) -> Result<CellInvite> {
        let node = {
            let state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            Arc::clone(&state.node)
        };
        let membership = node.cell_membership(identity, cell).await?;
        if !membership
            .member(&identity)
            .is_some_and(|member| member.state.member)
        {
            return Err(UnknownCell { cell }.into());
        }
        let mut state = self.runtime.state.lock().await;
        let secret = state.pending_cell_invites.mint(
            (identity, cell),
            lifetime.unwrap_or(DEFAULT_INVITE_LIFETIME),
            Instant::now(),
        )?;
        Ok(CellInvite {
            version: CELL_INVITE_FORMAT_VERSION,
            inviter_addr: state.node.dial_handle().addr(),
            secret,
            cell,
        })
    }

    async fn join(&self, identity: PdnId, invite: CellInvite) -> Result<CellId> {
        if invite.version != CELL_INVITE_FORMAT_VERSION {
            return Err(UnsupportedCellInviteVersion {
                version: invite.version,
            }
            .into());
        }
        let cleanup_tasks = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            if !state.joining_in_flight.insert((identity, invite.cell)) {
                return Err(JoinInProgress {
                    identity,
                    cell: invite.cell,
                }
                .into());
            }
            state.cleanup_tasks.clone()
        };
        // Released synchronously on every outcome; the reservation's `Drop`
        // covers a cancellation of this future alone.
        let reservation = JoinReservation {
            state: Arc::clone(&self.runtime.state),
            key: (identity, invite.cell),
            armed: true,
            cleanup_tasks,
        };
        let joined = join_via_dialogue(&self.runtime.state, identity, &invite).await;
        reservation.release().await;
        joined
    }
}

/// The joiner's half: dial, run the dialogue, then record both tickets and
/// the directory's entry before the catch-up the join waits for.
async fn join_via_dialogue(
    state: &Arc<Mutex<State>>,
    identity: PdnId,
    invite: &CellInvite,
) -> Result<CellId> {
    let (node, keys, author) = {
        let state = state.lock().await;
        let hosted = state.hosted(identity)?;
        let keys = hosted
            .directory
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
            dial.connect(invite.inviter_addr.clone(), CELL_JOIN_ALPN)
                .await
                .context(InviterUnreachable)?,
        )
    };
    let request = JoinRequest {
        version: CELL_INVITE_FORMAT_VERSION,
        secret: invite.secret,
        joiner: identity,
        announcement_key: keys.public_key(),
    };
    let device = MemberDevice {
        node: node.node_id(),
        author,
    };
    let accept = |offer: &JoinOffer| {
        let join_statement = offer.write_joined.then(|| {
            keys.join_statement(&invite.cell, Seq::new(offer.seq))
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

    let cell = invite.cell;
    let caught_up = node
        .import_cell(
            identity,
            cell,
            CellTickets {
                membership: tickets.membership,
                records: tickets.records,
            },
        )
        .await?;
    {
        let state = state.lock().await;
        let directory = &state.hosted(identity)?.directory;
        record_cell(&node, directory, identity, cell, Seq::new(offer.seq)).await?;
    }
    caught_up.wait(JOIN_CATCH_UP_TIMEOUT).await?;
    Ok(cell)
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
    if request.version != CELL_INVITE_FORMAT_VERSION
        || pdn_id_of(&request.announcement_key) != request.joiner
    {
        return None;
    }
    let (node, identity, cell) = {
        let mut state = state.lock().await;
        // Before any state change.
        let (identity, cell) = state
            .pending_cell_invites
            .verify_and_burn(&request.secret, Instant::now())?;
        (Arc::clone(&state.node), identity, cell)
    };
    let membership = node.cell_membership(identity, cell).await.ok()?;
    let inviter = membership.member(&identity)?;
    if !inviter.state.member {
        return None;
    }
    let actor_seq = inviter.run();
    let known = membership.member(&request.joiner);
    let (seq, write_joined) = match known {
        Some(member) => match member.joined_at() {
            Some(joined) => (joined, false),
            None => (member.run() + 1, true),
        },
        None => (1, true),
    };
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
            || !join_verifies(&request.joiner, &cell, Seq::new(seq), &joined)
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
        node.write_cell_entry(
            identity,
            cell,
            CellStore::Membership,
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
        node.write_cell_entry(
            identity,
            cell,
            CellStore::Membership,
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
        .share_cell_tickets(identity, cell, AddrInfoOptions::RelayAndAddresses)
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

/// The identity holds `cell` from `seq` of its chain on: both stores' write
/// tickets and the cell's entry in its directory (cells D35), which reach
/// its other devices.
async fn record_cell(
    node: &data_layer::SyncNode,
    directory: &data_layer::PrivateMetadataStore,
    identity: PdnId,
    cell: CellId,
    seq: Seq,
) -> Result<()> {
    let tickets = node
        .share_cell_tickets(identity, cell, AddrInfoOptions::RelayAndAddresses)
        .await?;
    directory
        .put_ticket(
            &cell_ticket_kind(&cell, CellStore::Membership),
            &tickets.membership,
        )
        .await?;
    directory
        .put_ticket(
            &cell_ticket_kind(&cell, CellStore::Records),
            &tickets.records,
        )
        .await?;
    directory.record_cell(cell, seq).await
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

/// Reserves an `(identity, cell)` pair against a concurrent `join`.
struct JoinReservation {
    state: Arc<Mutex<State>>,
    key: (PdnId, CellId),
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

/// Discards a cell `create` brought up if it fails, or is dropped, before
/// the cell is recorded.
struct CellRollback {
    node: Arc<data_layer::SyncNode>,
    identity: PdnId,
    cell: CellId,
    armed: bool,
    cleanup_tasks: CleanupSupervisor,
}

impl CellRollback {
    fn new(
        node: Arc<data_layer::SyncNode>,
        identity: PdnId,
        cell: CellId,
        cleanup_tasks: CleanupSupervisor,
    ) -> Self {
        Self {
            node,
            identity,
            cell,
            armed: true,
            cleanup_tasks,
        }
    }

    async fn roll_back(&mut self) {
        let _ = self.node.discard_cell(self.identity, self.cell).await;
        self.disarm();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CellRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let node = Arc::clone(&self.node);
        let (identity, cell) = (self.identity, self.cell);
        self.cleanup_tasks.spawn(async move {
            let _ = node.discard_cell(identity, cell).await;
        });
    }
}
