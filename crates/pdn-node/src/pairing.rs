//! The pairing protocol (ADR-0011): how two identities that share nothing
//! become connected.
//!
//! One raw bidirectional exchange per establishment on the dedicated
//! pairing ALPN — not a document-sync session. The inviter mints a one-time
//! secret and a self-contained [`InvitePayload`]; the scanner dials the
//! payload's address and presents the secret together with its half of the
//! connection state; the inviter atomically verifies-and-burns the secret
//! *before any state change* and answers with its own half. Both sides then
//! assemble the same state, mirrored: a connections record in the
//! private-metadata directory, the metadata pair
//! ([`data_layer::ConnectionMetadata`]), and the pair's tickets in the same
//! directory — which is how the connection reaches the identities' other
//! devices.
//!
//! Refusals are uniform: whatever the reason — unknown secret, expired,
//! already burned, malformed request, unsupported version — the inviter
//! closes the connection without a distinguishing answer, and a refused
//! attempt leaves no observable state. A wrong secret burns nothing: a
//! guess cannot extinguish a ceremony in progress.
//!
//! The dialogue carries no KERI proof of control over a presented `PdnId`
//! — deferred (ADR-0008): the exchange is bearer-level, secret plus
//! tickets. Both peers must be online: there are no pending invitations.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use data_layer::{
    own_ticket_kind, peer_ticket_kind, AcceptError, AddrInfoOptions, Connection,
    ConnectionMetadata, ConnectionMetadataStore, DocTicket, EndpointAddr, ProtocolHandler,
    RecvStream, SendStream, ShareMode,
};
use pdn_types::PdnId;
use rand::{rngs::SysRng, TryRng as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::runtime::State;

/// The dedicated pairing ALPN — the protocol the runtime registers at spawn
/// next to the built-in stack and the dial side connects under.
pub(crate) const PAIRING_ALPN: &[u8] = b"/pdn/pairing/0";

/// The invite payload format this runtime speaks. A scanner handed a
/// payload with any other version refuses it before dialing; the inviter
/// likewise refuses a request carrying an unknown version (uniformly, like
/// every other refusal).
pub const INVITE_FORMAT_VERSION: u8 = 0;

/// How long a pending invite lives unless the invite overrides it.
pub(crate) const DEFAULT_INVITE_LIFETIME: Duration = Duration::from_secs(120);

/// Ceiling on one length-prefixed wire message, shared by both ceremonies
/// on this framing (pairing and linking). Their dialogue messages carry
/// ids, one address, and one or two tickets — far below this; the bound
/// exists so a malformed length prefix cannot demand an unbounded read.
pub(crate) const MAX_WIRE_MESSAGE_LEN: u32 = 64 * 1024;

/// The self-contained invite payload — what the inviter's device shows and
/// the scanner's device consumes. In-process it travels as a value; its
/// string/QR encoding is a host concern.
///
/// Deliberately bearer-free: a format version, the inviter device's node
/// address (the dial target), the one-time secret, and the inviting
/// identity's `PdnId` — no tickets and no identity proof. The payload is
/// semi-public (shown on a screen, photographable), so nothing in it may
/// grant durable access; the secret it carries is one-time and short-lived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePayload {
    /// Payload format version ([`INVITE_FORMAT_VERSION`]).
    pub version: u8,
    /// The inviter device's node address — where the scanner dials.
    pub inviter_addr: EndpointAddr,
    /// The one-time pairing secret, pending on the inviting runtime.
    pub secret: [u8; 32],
    /// The inviting identity.
    pub inviter: PdnId,
}

/// `establish` was handed an invite payload whose format version this
/// runtime does not speak; refused before dialing. Downcast from the
/// `anyhow::Error` of the connections service's `establish`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported invite payload version: {version}")]
pub struct UnsupportedInviteVersion {
    /// The version the payload carried.
    pub version: u8,
}

/// The scanner's half of the dialogue: the secret, who is scanning, where
/// to reach it, and the read ticket to the metadata store it issues toward
/// the inviter.
#[derive(Debug, Serialize, Deserialize)]
struct PairingRequest {
    version: u8,
    secret: [u8; 32],
    scanner: PdnId,
    scanner_addr: EndpointAddr,
    ticket: DocTicket,
}

/// The inviter's half, sent only after the verify-and-burn and the state
/// assembly: the read ticket to the metadata store it issues toward the
/// scanner.
#[derive(Debug, Serialize, Deserialize)]
struct PairingResponse {
    ticket: DocTicket,
}

/// One pending invite: the identity it invites for and when it expires.
#[derive(Debug, Clone, Copy)]
struct PendingInvite {
    identity: PdnId,
    expires_at: Instant,
}

/// A pending-invite set of one runtime, keyed by secret bytes, each bound
/// to the identity it invites for. Lives inside the runtime state — one
/// instance per ceremony that mints one-time secrets (pairing here, device
/// linking in [`crate::linking`]) — so every operation on it: insertion at
/// invite, the verify-and-burn at presentation — is a map operation under
/// the runtime's one coarse lock.
///
/// Expiry is lazy: checked at presentation, swept at the next invite — no
/// background task. Runtime restart drops the set; an invite is a live
/// ceremony, and a fresh invite is the recovery path for every failure.
#[derive(Debug, Default)]
pub(crate) struct PendingInvites {
    map: HashMap<[u8; 32], PendingInvite>,
}

impl PendingInvites {
    /// Mint a fresh one-time secret for `identity` with `lifetime`, sweeping
    /// invites that have already expired. The secret is 32 bytes from the
    /// operating-system generator.
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

    /// The atomic verify-and-burn: present and unexpired — removed, and the
    /// invited identity returned; expired — removed and refused; unknown —
    /// refused, burning nothing (a wrong guess cannot extinguish a live
    /// invite). One map operation, run under the runtime lock *before* any
    /// state is created or written.
    pub(crate) fn verify_and_burn(&mut self, secret: &[u8; 32], now: Instant) -> Option<PdnId> {
        // Peek first: removal must only happen for the presented secret
        // itself — a miss must not disturb the map.
        let live = self.map.get(secret)?.expires_at > now;
        let pending = self.map.remove(secret)?;
        live.then_some(pending.identity)
    }
}

/// A clonable slot for the runtime state the pairing handler serves: filled
/// once, right after the node spawns (the handler is built before the node
/// exists), and held weakly so the runtime's shutdown can reclaim sole
/// ownership of the state. A connection arriving before the slot is filled
/// is refused — unreachable in the honest flow, because no invite payload
/// exists before spawn returns.
pub(crate) type StateSlot = Arc<OnceLock<Weak<Mutex<State>>>>;

/// The accept side of the pairing dialogue, registered at `Runtime::spawn`
/// through the data-layer assembly slot.
#[derive(Debug, Clone)]
pub(crate) struct PairingHandler {
    state: StateSlot,
}

impl PairingHandler {
    /// A handler with an unfilled state slot; [`Runtime::spawn`] fills the
    /// slot right after the node comes up.
    ///
    /// [`Runtime::spawn`]: crate::Runtime::spawn
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
        }
    }

    /// The slot to fill with the spawned runtime's state.
    pub(crate) fn slot(&self) -> StateSlot {
        Arc::clone(&self.state)
    }

    /// Run the inviter's side of one establishment. `None` is a refusal —
    /// any reason at all — answered by the caller with the one uniform
    /// close. `Some(())` means the dialogue completed and the response was
    /// sent.
    async fn serve(&self, connection: &Connection) -> Option<()> {
        let (mut send, mut recv) = connection.accept_bi().await.ok()?;
        let request: PairingRequest = read_message(&mut recv).await.ok()?;
        if request.version != INVITE_FORMAT_VERSION {
            return None;
        }

        // The runtime state is held only for the local verify-and-assemble,
        // inside this block: both the guard and the strong `Arc` drop at its
        // end, before the network reply below. So a `shutdown` racing this
        // accept reclaims sole ownership as soon as the local work finishes,
        // never waiting on the dialer to close — were the `Arc` held across
        // `connection.closed()`, `shutdown` would busy-spin until the
        // transport's idle timeout.
        let response_ticket = {
            // The late-bound slot: unfilled (no invite can exist yet) or a
            // runtime already gone both refuse.
            let state = self.state.get()?.upgrade()?;
            let mut state = state.lock().await;

            // The atomic verify-and-burn, before any state change. Everything
            // below only runs for a live, unburned secret.
            let identity = state
                .pending_invites
                .verify_and_burn(&request.secret, Instant::now())?;

            // Assembly, mirroring the dial side: create-or-reuse own, import
            // the scanner's ticket as peer (its node address supplements the
            // ticket's own first-sync contacts), connections entry, directory
            // kinds.
            let (own, created_fresh) = own_store_toward(&state, identity, request.scanner)
                .await
                .ok()?;
            let Ok(ticket) = own
                .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
                .await
            else {
                // Sharing failed before assemble committed: forget a freshly
                // created own so a failed accept leaves no orphan, the same
                // rollback the dial side does.
                if created_fresh {
                    let _ = state.node.forget_doc(own.namespace()).await;
                }
                return None;
            };
            assemble_connection(
                &mut state,
                identity,
                request.scanner,
                own,
                request.ticket,
                Some(request.scanner_addr),
            )
            .await
            .ok()?;
            ticket
        };

        // Commit precedes the reply: if the response is lost, the inviter
        // keeps its half and a fresh invite converges the rest
        // (re-establishment).
        write_message(
            &mut send,
            &PairingResponse {
                ticket: response_ticket,
            },
        )
        .await
        .ok()?;
        send.finish().ok()?;
        // Hold the connection until the dialer closes it, so the response
        // is not cut off by dropping this side first.
        connection.closed().await;
        Some(())
    }
}

impl ProtocolHandler for PairingHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if self.serve(&connection).await.is_none() {
            // The one uniform refusal, whatever the reason: close without a
            // distinguishing answer, so a prober cannot separate wrong from
            // expired from already-burned — or any other cause.
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }
}

/// The scanner's side of one establishment: dial the payload's address on
/// the pairing ALPN, run the exchange, and assemble the same connection
/// state as the accept side, mirrored.
///
/// The runtime lock is taken *per phase* and never held across the network
/// round-trip — mirroring the accept side, which reads the request before
/// locking and writes the response after unlocking. Holding it across the
/// dial and the response wait would deadlock: the accept side (this runtime
/// included) needs that same lock to answer, so two runtimes establishing
/// toward each other would each wait on a lock the other holds. Takes the
/// shared [`Mutex`] rather than a guard so it can lock and unlock around
/// each phase.
pub(crate) async fn establish_via_dialogue(
    state: &Mutex<State>,
    identity: PdnId,
    payload: &InvitePayload,
) -> Result<()> {
    // A brief lock for the hosted check and the dial handle (a cheap
    // snapshot); released before any network I/O.
    let dial = {
        let state = state.lock().await;
        state.hosted(identity)?;
        state.node.dial_handle()
    };

    // Network — no lock held. Dial before minting `own`, so an unreachable
    // inviter never leaves a replica behind.
    let connection = dial
        .connect(payload.inviter_addr.clone(), PAIRING_ALPN)
        .await
        .context("could not reach the inviter")?;

    // Reachable: this side's half of the pair, created (or reused) under a
    // brief lock. Its read ticket rides in the request; the lock is released
    // before the round-trip.
    let (own, created_fresh) = {
        let state = state.lock().await;
        own_store_toward(&state, identity, payload.inviter).await?
    };

    // The round-trip — no lock held, so the accept side can take the lock to
    // answer (this is what breaks the reciprocal-establishment deadlock). Any
    // failure here is before assemble commits, so a freshly created `own` is
    // an orphan — unreferenced by directory or cache — that the reconcile
    // pass would re-sync for the node's life; forget it instead.
    let response: Result<PairingResponse> = async {
        let ticket = own
            .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
            .await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        write_message(
            &mut send,
            &PairingRequest {
                version: INVITE_FORMAT_VERSION,
                secret: payload.secret,
                scanner: identity,
                scanner_addr: dial.addr(),
                ticket,
            },
        )
        .await?;
        send.finish()?;
        // Refusals are uniform by design: the connection just closes, and
        // this read fails without saying why.
        read_message(&mut recv)
            .await
            .context("establishment refused by the inviter")
    }
    .await;
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            if created_fresh {
                let state = state.lock().await;
                let _ = state.node.forget_doc(own.namespace()).await;
            }
            return Err(err);
        }
    };
    connection.close(0u32.into(), b"done");

    // Commit under a brief lock. The inviter's address is a live first-sync
    // contact for the imported pair, exactly as the scanner's address is on
    // the accept side.
    let mut state = state.lock().await;
    assemble_connection(
        &mut state,
        identity,
        payload.inviter,
        own,
        response.ticket,
        Some(payload.inviter_addr.clone()),
    )
    .await
}

/// Create-or-reuse this side's own metadata store toward `peer`: the
/// cached pair first, then the directory's own-kind write ticket — so
/// re-establishment and linked devices converge on one replica — and only
/// then a fresh replica. The identity must be hosted here.
///
/// The bool is `true` only when a fresh replica was created — so the caller
/// can forget it if establishment then fails before commit, while a reused
/// or directory-imported replica (which other devices depend on) survives.
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
            ConnectionMetadataStore::import(&state.node, write_ticket).await?,
            false,
        )),
        None => Ok((ConnectionMetadataStore::create(&state.node).await?, true)),
    }
}

/// The post-dialogue assembly, identical on both sides: import the received
/// read ticket as `peer` (`peer_addr` supplementing its first-sync
/// contacts), record the counterparty among the directory's connections
/// records, publish the pair's tickets in the same directory under the
/// per-connection kinds, and cache the pair for the grant surface.
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
    // Reuse the cached peer store when the ticket still addresses the same
    // replica: re-establishment carries the counterpart's same namespace, and
    // a fresh import would leak a tracked doc and an author every attempt
    // (own is reused the same way in `own_store_toward`). A genuinely new
    // peer namespace still imports.
    let peer_store = match state.metadata_pairs.get(&(identity, peer)) {
        Some(pair) if pair.peer.namespace() == peer_ticket.capability.id() => pair.peer.clone(),
        _ => ConnectionMetadataStore::import(&state.node, peer_ticket.clone()).await?,
    };

    // The directory carries the pair to the identity's other devices: the
    // write ticket to `own` (every device of the issuer writes grants), the
    // received read ticket to `peer`. Only routing lives here — the grants
    // themselves stay in the metadata stores.
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

    // This device asserts itself into `own` (the counterparty resolves
    // callers through these records), and the pair registers for session
    // classification. Assert-once, like every pair opening: a
    // re-establishment onto a reused store leaves a live record untouched
    // and never resurrects a withdrawn one.
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

/// Write one length-prefixed postcard message.
pub(crate) async fn write_message<T: Serialize>(send: &mut SendStream, message: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(message)?;
    let len = u32::try_from(bytes.len()).context("wire message too large")?;
    if len > MAX_WIRE_MESSAGE_LEN {
        anyhow::bail!("wire message too large: {len} bytes");
    }
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

/// Read one length-prefixed postcard message, refusing lengths beyond
/// [`MAX_WIRE_MESSAGE_LEN`].
pub(crate) async fn read_message<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
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
