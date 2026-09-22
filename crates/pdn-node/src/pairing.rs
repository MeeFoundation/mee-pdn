//! The pairing protocol (ADR-0011): one raw exchange on the pairing ALPN.
//! The inviter verifies-and-burns the secret before any state change and
//! answers with its half; both sides then assemble the same state,
//! mirrored. Refusals are uniform — one close, whatever the reason — and a
//! wrong secret burns nothing. Bearer-level: no KERI proof of the presented
//! `PdnId` (ADR-0008).

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use data_layer::{
    own_ticket_kind, peer_ticket_kind, AcceptError, AddrInfoOptions, Connection,
    ConnectionMetadata, ConnectionMetadataStore, DocTicket, EndpointAddr, ProtocolHandler,
    ShareMode,
};
use pdn_types::PdnId;
use rand::{rngs::SysRng, TryRng as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::Mutex,
};

use crate::runtime::State;

pub(crate) const PAIRING_ALPN: &[u8] = b"/pdn/pairing/0";

/// Any other version is refused before dialing by the scanner, and
/// uniformly by the inviter.
pub const INVITE_FORMAT_VERSION: u8 = 0;

pub(crate) const DEFAULT_INVITE_LIFETIME: Duration = Duration::from_mins(2);

/// Ceiling on one length-prefixed wire message of both ceremonies, so a
/// malformed length prefix cannot demand an unbounded read.
pub(crate) const MAX_WIRE_MESSAGE_LEN: u32 = 64 * 1024;

/// The invite payload: bearer-free on purpose, since it is shown on a
/// screen and photographable. Its string/QR encoding is a host concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitePayload {
    pub version: u8,
    /// Where the scanner dials.
    pub inviter_addr: EndpointAddr,
    pub secret: [u8; 32],
    pub inviter: PdnId,
}

/// Refused before dialing. Downcast from the `anyhow::Error` of
/// `establish`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported invite payload version: {version}")]
pub struct UnsupportedInviteVersion {
    pub version: u8,
}

/// The dialogue reached the inviter and ended without an answer.
/// Reasonless by design; a connection that dies once the request is away,
/// or a handler that fails internally, surfaces the same way. Distinct from
/// [`InviterUnreachable`], whose failure precedes this point. Downcast from
/// the `anyhow::Error` of `establish`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("establishment refused by the inviter")]
pub struct EstablishmentRefused;

/// The exchange was still in flight when [`ESTABLISHMENT_DIALOGUE_TIMEOUT`]
/// passed — a hung counterparty, not a refusing one. Downcast from the
/// `anyhow::Error` of `establish`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("establishment dialogue did not complete in time")]
pub struct EstablishmentTimeout;

/// A constant because `establish` names no budget; without it a hung
/// inviter holds the caller for the transport's idle timeout.
pub const ESTABLISHMENT_DIALOGUE_TIMEOUT: Duration = Duration::from_secs(15);

/// The dial reached no inviting device — before any dialogue, so neither a
/// refusal nor a timeout. One type for both ceremonies. Downcast from the
/// `anyhow::Error` of `establish` and `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("could not reach the inviter")]
pub struct InviterUnreachable;

/// Another `establish` toward the same pair is in flight; refused before
/// dialing. Downcast from the `anyhow::Error` of `establish`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("an establishment toward {peer} from {identity} is already in flight on this runtime")]
pub struct EstablishmentInProgress {
    pub identity: PdnId,
    pub peer: PdnId,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairingRequest {
    version: u8,
    secret: [u8; 32],
    scanner: PdnId,
    scanner_addr: EndpointAddr,
    /// The read ticket to the store the scanner issues toward the inviter.
    ticket: DocTicket,
}

/// Sent only after the verify-and-burn and the state assembly.
#[derive(Debug, Serialize, Deserialize)]
struct PairingResponse {
    ticket: DocTicket,
}

#[derive(Debug, Clone, Copy)]
struct PendingInvite {
    identity: PdnId,
    expires_at: Instant,
}

/// One instance per ceremony that mints one-time secrets, inside the
/// runtime state so every operation is a map operation under the coarse
/// lock. Expiry is lazy: checked at presentation, swept at the next invite.
#[derive(Debug, Default)]
pub(crate) struct PendingInvites {
    map: HashMap<[u8; 32], PendingInvite>,
}

impl PendingInvites {
    /// 32 bytes from the operating-system generator.
    pub(crate) fn mint(
        &mut self,
        identity: PdnId,
        lifetime: Duration,
        now: Instant,
    ) -> Result<[u8; 32]> {
        self.map.retain(|_, pending| pending.expires_at > now);
        let mut secret = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut secret)
            .context("operating-system randomness unavailable")?;
        self.map.insert(
            secret,
            PendingInvite {
                identity,
                expires_at: now + lifetime,
            },
        );
        Ok(secret)
    }

    /// Present and unexpired — burned and returned; expired — burned and
    /// refused; unknown — refused, burning nothing.
    pub(crate) fn verify_and_burn(&mut self, secret: &[u8; 32], now: Instant) -> Option<PdnId> {
        // Peek first: a miss must not disturb the map.
        let live = self.map.get(secret)?.expires_at > now;
        let pending = self.map.remove(secret)?;
        live.then_some(pending.identity)
    }
}

/// Filled once, right after the node spawns (the handler is built before
/// the node exists), and held weakly so a handler clone does not keep the
/// state alive. A connection arriving before the slot is filled is refused.
pub(crate) type StateSlot = Arc<OnceLock<Weak<Mutex<State>>>>;

/// Never meant to bound anything: exists so `shutdown`'s `acquire_many` has
/// a fixed permit count to wait for.
pub(crate) const MAX_CONCURRENT_ESTABLISHMENTS: usize = 1_048_576;
const MAX_CONCURRENT_ESTABLISHMENTS_U32: u32 = 1_048_576;

/// How long `shutdown` waits for in-flight `accept` calls before letting
/// the router close the endpoint anyway.
pub const SHUTDOWN_ESTABLISHMENT_BUDGET: Duration = Duration::from_secs(10);

/// The accept side of the pairing dialogue.
#[derive(Debug, Clone)]
pub(crate) struct PairingHandler {
    state: StateSlot,
    /// One permit per `accept` in flight; `shutdown` waits for every permit
    /// rather than letting the router abort an establishment outright.
    in_flight: Arc<tokio::sync::Semaphore>,
}

impl PairingHandler {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ESTABLISHMENTS)),
        }
    }

    pub(crate) fn slot(&self) -> StateSlot {
        Arc::clone(&self.state)
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn in_flight_probe(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.in_flight)
    }

    /// `None` is a refusal, any reason at all, answered by the caller with
    /// the one uniform close.
    async fn serve(&self, connection: &Connection) -> Option<()> {
        let (mut send, mut recv) = connection.accept_bi().await.ok()?;
        let state = self.state.get()?.upgrade()?;
        serve_pairing(&state, &mut send, &mut recv).await?;
        send.finish().ok()?;
        // Held until the dialer closes, so the response is not cut off.
        connection.closed().await;
        Some(())
    }
}

/// The inviter's half of the dialogue, over any pair of streams: read
/// the request, verify and burn the secret before any state change,
/// assemble this side's half of the connection, and answer with its
/// ticket. `None` is a refusal, any reason at all; the transport answers
/// it its own way.
pub(crate) async fn serve_pairing<R, W>(
    state_arc: &Arc<Mutex<State>>,
    send: &mut W,
    recv: &mut R,
) -> Option<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let request: PairingRequest = read_message(recv).await.ok()?;
    if request.version != INVITE_FORMAT_VERSION {
        return None;
    }

    // The state is held only for the local verify-and-assemble: the guard
    // drops before the reply, so no other lock holder waits on the dialer.
    let response_ticket = {
        let mut state = state_arc.lock().await;

        // Before any state change.
        let identity = state
            .pending_invites
            .verify_and_burn(&request.secret, Instant::now())?;

        let (own, created_fresh) = own_store_toward(&state, identity, request.scanner)
            .await
            .ok()?;
        // Armed as soon as a fresh replica might exist: a cancellation of
        // this future from here on forgets it.
        let mut rollback = EstablishGuard::new(
            Arc::clone(state_arc),
            identity,
            own.namespace(),
            created_fresh,
            state.cleanup_tasks.clone(),
        );
        let Ok(ticket) = own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await
        else {
            if created_fresh {
                let _ = state.node.forget_doc(identity, own.namespace()).await;
            }
            rollback.disarm();
            return None;
        };
        let own_namespace = own.namespace();
        let result = assemble_connection(
            &mut state,
            identity,
            request.scanner,
            own,
            request.ticket,
            Some(request.scanner_addr),
        )
        .await;
        if let Ok(()) = result {
            rollback.disarm();
        } else {
            if created_fresh {
                let _ = state.node.forget_doc(identity, own_namespace).await;
            }
            rollback.disarm();
            return None;
        }
        ticket
    };

    // Commit precedes the reply: a lost response leaves the inviter's
    // half, and a fresh invite converges the rest.
    write_message(
        send,
        &PairingResponse {
            ticket: response_ticket,
        },
    )
    .await
    .ok()?;
    Some(())
}

impl ProtocolHandler for PairingHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // `acquire` only errs once the semaphore is closed, which never
        // happens: the `let else` is exhaustiveness.
        let Ok(_permit) = self.in_flight.acquire().await else {
            return Ok(());
        };
        if self.serve(&connection).await.is_none() {
            // The one uniform refusal.
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }

    /// `Router::shutdown` stops dispatching new `accept` calls before
    /// awaiting this, so `acquire_many` reaching every permit means every
    /// `accept` in flight when shutdown began has returned.
    async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            SHUTDOWN_ESTABLISHMENT_BUDGET,
            self.in_flight
                .acquire_many(MAX_CONCURRENT_ESTABLISHMENTS_U32),
        )
        .await;
    }
}

/// The scanner's side. The runtime lock is taken per phase and never held
/// across the round-trip: the accept side (this runtime included) needs
/// the same lock to answer, so two runtimes establishing toward each other
/// would otherwise deadlock.
pub(crate) async fn establish_via_dialogue(
    state: &Arc<Mutex<State>>,
    identity: PdnId,
    payload: &InvitePayload,
) -> Result<()> {
    let (dial, cleanup_tasks) = {
        let mut state_guard = state.lock().await;
        state_guard.hosted(identity)?;
        if !state_guard
            .establishing_in_flight
            .insert((identity, payload.inviter))
        {
            return Err(EstablishmentInProgress {
                identity,
                peer: payload.inviter,
            }
            .into());
        }
        (
            state_guard.node.dial_handle(),
            state_guard.cleanup_tasks.clone(),
        )
    };
    // Released synchronously on every outcome, so a caller retrying at once
    // never races a detached release; the reservation's `Drop` covers a
    // cancellation of this function alone.
    let reservation = EstablishReservation::new(
        Arc::clone(state),
        identity,
        payload.inviter,
        cleanup_tasks.clone(),
    );
    let result = establish_via_dialogue_inner(state, identity, payload, dial, cleanup_tasks).await;
    reservation.release().await;
    result
}

/// What a pipe between two identities of one node buffers each way: one
/// framed message at [`MAX_WIRE_MESSAGE_LEN`] plus its length prefix, so
/// neither half blocks on the other's read.
#[allow(clippy::as_conversions)] // const context, and a 32-bit length fits every usize we build for
const PAIRING_PIPE_BYTES: usize = MAX_WIRE_MESSAGE_LEN as usize + 4;

/// The dialogue between two identities of one node, run over a pipe: the
/// same messages, the same verify-and-burn and the same assembly as
/// between two nodes, with the serving half taken from this runtime's own
/// state.
async fn pair_in_process(
    state: &Arc<Mutex<State>>,
    request: &PairingRequest,
) -> Result<PairingResponse> {
    let (dialing, serving) = tokio::io::duplex(PAIRING_PIPE_BYTES);
    let (mut dial_recv, mut dial_send) = tokio::io::split(dialing);
    let (mut serve_recv, mut serve_send) = tokio::io::split(serving);
    let served = {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            serve_pairing(&state, &mut serve_send, &mut serve_recv)
                .await
                .is_some()
        })
    };
    write_message(&mut dial_send, request).await?;
    let response = read_message(&mut dial_recv).await;
    // The serving half's own outcome decides: a read that failed because
    // it declined must not read as a broken pipe.
    match served.await {
        Ok(true) => response.context(EstablishmentRefused),
        _ => Err(EstablishmentRefused.into()),
    }
}

/// Factored out so the reservation releases synchronously around every
/// exit, `?` early returns included.
#[allow(clippy::too_many_lines)] // one dialogue, both transports and the rollback in one place
async fn establish_via_dialogue_inner(
    state: &Arc<Mutex<State>>,
    identity: PdnId,
    payload: &InvitePayload,
    dial: data_layer::DialHandle,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
) -> Result<()> {
    // An invite whose address carries this node's own wire identity is an
    // invite from a co-located identity: iroh refuses a connection to its
    // own endpoint id, so the dialogue runs over a pipe. Dial before
    // minting `own`, so an unreachable inviter leaves no replica.
    let connection = if payload.inviter_addr.id == dial.id() {
        None
    } else {
        Some(
            dial.connect(payload.inviter_addr.clone(), PAIRING_ALPN)
                .await
                .context(InviterUnreachable)?,
        )
    };

    let (own, created_fresh) = {
        let state = state.lock().await;
        own_store_toward(&state, identity, payload.inviter).await?
    };
    // The explicit cleanup below covers only a completed round-trip that
    // came back `Err`; a cancellation skips it, and this guard is what
    // forgets a fresh `own` then.
    let mut rollback = EstablishGuard::new(
        Arc::clone(state),
        identity,
        own.namespace(),
        created_fresh,
        cleanup_tasks,
    );

    // The ticket is minted outside the ceiling, so the ceiling bounds
    // exactly the exchange with the peer.
    let ticket = own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await;
    // No lock held across the round-trip.
    let response: Result<PairingResponse> = match ticket {
        Err(err) => Err(err),
        Ok(ticket) => {
            let request = PairingRequest {
                version: INVITE_FORMAT_VERSION,
                secret: payload.secret,
                scanner: identity,
                scanner_addr: dial.addr(),
                ticket,
            };
            match tokio::time::timeout(ESTABLISHMENT_DIALOGUE_TIMEOUT, async {
                match &connection {
                    Some(connection) => {
                        let (mut send, mut recv) = connection.open_bi().await?;
                        write_message(&mut send, &request).await?;
                        send.finish()?;
                        // A refusal is just the connection closing.
                        read_message(&mut recv).await.context(EstablishmentRefused)
                    }
                    None => pair_in_process(state, &request).await,
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_ceiling_passed) => Err(EstablishmentTimeout.into()),
            }
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            if created_fresh {
                let state = state.lock().await;
                let _ = state.node.forget_doc(identity, own.namespace()).await;
            }
            rollback.disarm();
            return Err(err);
        }
    };
    if let Some(connection) = &connection {
        connection.close(0u32.into(), b"done");
    }

    let mut state_guard = state.lock().await;
    let own_namespace = own.namespace();
    let result = assemble_connection(
        &mut state_guard,
        identity,
        payload.inviter,
        own,
        response.ticket,
        Some(payload.inviter_addr.clone()),
    )
    .await;
    match result {
        Ok(()) => {
            rollback.disarm();
            Ok(())
        }
        Err(err) => {
            if created_fresh {
                let _ = state_guard.node.forget_doc(identity, own_namespace).await;
            }
            rollback.disarm();
            Err(err)
        }
    }
}

/// Reserves an `(identity, peer)` pair against a concurrent `establish`.
/// Held for the whole function, unlike [`EstablishGuard`]: a cancellation
/// during the dial, before any rollback guard exists, must still release
/// it.
struct EstablishReservation {
    state: Arc<Mutex<State>>,
    identity: PdnId,
    peer: PdnId,
    armed: bool,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
}

impl EstablishReservation {
    fn new(
        state: Arc<Mutex<State>>,
        identity: PdnId,
        peer: PdnId,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            state,
            identity,
            peer,
            armed: true,
            cleanup_tasks,
        }
    }

    /// The normal path: a caller retrying at once must see the pair free.
    async fn release(mut self) {
        self.state
            .lock()
            .await
            .establishing_in_flight
            .remove(&(self.identity, self.peer));
        self.armed = false;
    }
}

impl Drop for EstablishReservation {
    /// Cancellation only. `Drop` is synchronous and the removal is not, so
    /// it is spawned detached; a cancelled attempt has no caller waiting to
    /// retry at once.
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = Arc::clone(&self.state);
        let identity = self.identity;
        let peer = self.peer;
        self.cleanup_tasks.spawn(async move {
            state
                .lock()
                .await
                .establishing_in_flight
                .remove(&(identity, peer));
        });
    }
}

/// Forgets a freshly created `own` replica if the establishing future is
/// dropped before it disarms. `Drop` is synchronous and the forget is not,
/// so it is spawned detached.
struct EstablishGuard {
    state: Arc<Mutex<State>>,
    identity: PdnId,
    own_namespace: data_layer::NamespaceId,
    created_fresh: bool,
    armed: bool,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
}

impl EstablishGuard {
    fn new(
        state: Arc<Mutex<State>>,
        identity: PdnId,
        own_namespace: data_layer::NamespaceId,
        created_fresh: bool,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            state,
            identity,
            own_namespace,
            created_fresh,
            armed: true,
            cleanup_tasks,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EstablishGuard {
    fn drop(&mut self) {
        if !self.armed || !self.created_fresh {
            return;
        }
        let state = Arc::clone(&self.state);
        let identity = self.identity;
        let own_namespace = self.own_namespace;
        self.cleanup_tasks.spawn(async move {
            let node = Arc::clone(&state.lock().await.node);
            let _ = node.forget_doc(identity, own_namespace).await;
        });
    }
}

/// The cached pair first, then the directory's own-kind write ticket — so
/// re-establishment and linked devices converge on one replica — and only
/// then a fresh replica. The bool is `true` only for a fresh replica, the
/// one a failed establishment may forget; a reused one other devices
/// depend on.
async fn own_store_toward(
    state: &State,
    identity: PdnId,
    peer: PdnId,
) -> Result<(ConnectionMetadataStore, bool)> {
    if let Some(pair) = state.metadata_pairs.get(&(identity, peer)) {
        return Ok((pair.own.clone(), false));
    }
    let directory = &state.hosted(identity)?.directory;
    match directory.get_ticket(&own_ticket_kind(&peer)).await? {
        Some(write_ticket) => Ok((
            ConnectionMetadataStore::import(&state.node, identity, write_ticket).await?,
            false,
        )),
        None => Ok((
            ConnectionMetadataStore::create(&state.node, identity).await?,
            true,
        )),
    }
}

/// The post-dialogue assembly, identical on both sides; `peer_addr`
/// supplements the imported ticket's first-sync contacts.
async fn assemble_connection(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
    own: ConnectionMetadataStore,
    peer_ticket: DocTicket,
    peer_addr: Option<EndpointAddr>,
) -> Result<()> {
    let mut peer_ticket = peer_ticket;
    if let Some(addr) = peer_addr {
        peer_ticket.nodes.push(addr);
    }
    // A cached peer store is reused while the ticket still addresses its
    // replica: a fresh import per re-establishment would leak a tracked doc.
    let peer_store = match state.metadata_pairs.get(&(identity, peer)) {
        Some(pair) if pair.peer.namespace() == peer_ticket.capability.id() => pair.peer.clone(),
        _ => ConnectionMetadataStore::import(&state.node, identity, peer_ticket.clone()).await?,
    };

    let own_write_ticket = own
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let directory = &state.hosted(identity)?.directory;
    directory
        .put_ticket(&own_ticket_kind(&peer), &own_write_ticket)
        .await?;
    directory
        .put_ticket(&peer_ticket_kind(&peer), &peer_ticket)
        .await?;
    directory.connect(peer).await?;

    // Assert-once, like every pair opening.
    own.ensure_device_published(state.node.node_id()).await?;
    state
        .node
        .host_connection(identity, peer, &own, &peer_store)?;

    state.metadata_pairs.insert(
        (identity, peer),
        ConnectionMetadata {
            own,
            peer: peer_store,
        },
    );
    Ok(())
}

/// Generic over its stream, so one implementation serves the network and
/// the pipe two identities of one node meet over (ADR-0013).
pub(crate) async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    send: &mut W,
    message: &T,
) -> Result<()> {
    let bytes = postcard::to_stdvec(message)?;
    let len = u32::try_from(bytes.len()).context("wire message too large")?;
    if len > MAX_WIRE_MESSAGE_LEN {
        anyhow::bail!("wire message too large: {len} bytes");
    }
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

/// Generic over its stream — see [`write_message`].
pub(crate) async fn read_message<R: AsyncRead + Unpin, T: DeserializeOwned>(
    recv: &mut R,
) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_WIRE_MESSAGE_LEN {
        anyhow::bail!("wire message too large: {len} bytes");
    }
    let mut bytes = vec![0u8; usize::try_from(len)?];
    recv.read_exact(&mut bytes).await?;
    Ok(postcard::from_bytes(&bytes)?)
}
