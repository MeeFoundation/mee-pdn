//! The device-linking protocol (ADR-0012): how a new device joins an
//! identity.
//!
//! One raw bidirectional exchange per linking on the dedicated linking
//! ALPN — establishment's shape (ADR-0011) applied to the higher-stakes
//! ceremony, and deliberately not a message variant of the pairing
//! protocol: what the reply hands over is a write ticket to the identity's
//! whole directory, not per-connection read tickets, so the two dialogues
//! evolve their wire formats independently. The inviting device mints a
//! one-time secret and a self-contained [`LinkingPayload`]; the new device
//! dials the payload's address and presents the secret; the inviter
//! atomically verifies-and-burns it *before any state change*, registers
//! the newcomer in its own directory replica as **pending** — the node id
//! taken from the connection's authenticated peer identity, never from a
//! claimed field — and answers with freshly minted write tickets to the
//! directory and the identity's data namespace. Registration precedes the
//! reply, and confers nothing on its own: the access book probes the
//! confirmed device set alone, so a response lost from there on leaves a
//! device that is visible to the identity's other devices and admitted
//! nowhere, and a fresh invite converges. The newcomer confirms itself once
//! the tickets are in hand — a write only a holder of the directory's write
//! ticket can make, which is why the inviter, unable to know its reply
//! arrived, does not make it.
//!
//! The dial side arms the identity for session classification the moment
//! its directory is imported — before the data namespace exists, so no
//! serving window opens on the long-lived namespace id — then imports both
//! tickets and returns caught up: one bounded wait for the first successful
//! directory sync session started after the import — not a retry loop; the
//! node's periodic reconcile pass is the re-dial cadence inside that wait's
//! budget. On any failure after the import it disarms the identity and
//! forgets both replicas, so a failed link leaves no residue and the
//! identity is unknown to the runtime again.
//!
//! Refusals are uniform: whatever the reason — unknown secret, expired,
//! already burned, malformed request, unsupported version — the inviter
//! closes the connection without a distinguishing answer, and a refused
//! attempt leaves no observable state. A wrong secret burns nothing.
//!
//! The dialogue carries no KERI proof of control over the identity —
//! deferred, exactly as in pairing (ADR-0008). Both devices must be online:
//! there are no pending linking invites.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use data_layer::{
    AcceptError, AddrInfoOptions, Connection, DocTicket, EndpointAddr, NamespaceId,
    NamespaceImport, PrivateMetadataStore, ProtocolHandler, ShareMode,
};
use pdn_types::{NodeId, PdnId};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    pairing::{read_message, write_message, StateSlot},
    runtime::{HostedIdentity, State},
};

/// The dedicated linking ALPN — the protocol the runtime registers at spawn
/// next to pairing and the built-in stack, and the dial side connects under.
pub(crate) const LINKING_ALPN: &[u8] = b"/pdn/linking/0";

/// The linking payload format this runtime speaks. A device handed a
/// payload with any other version refuses it before dialing; the inviter
/// likewise refuses a request carrying an unknown version (uniformly, like
/// every other refusal).
pub const LINKING_FORMAT_VERSION: u8 = 0;

/// The self-contained linking payload — what the inviting device shows and
/// the new device consumes. In-process it travels as a value; its string/QR
/// encoding is a host concern.
///
/// Deliberately bearer-free: a format version, the inviting device's node
/// address (the dial target), the one-time secret, and the identity's
/// `PdnId` — no tickets and no identity proof. The payload is semi-public
/// (shown on a screen, photographable), so nothing in it may grant durable
/// access; a photographed payload expires with its secret. The bootstrap
/// tickets ride the dialogue's encrypted reply instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkingPayload {
    /// Payload format version ([`LINKING_FORMAT_VERSION`]).
    pub version: u8,
    /// The inviting device's node address — where the new device dials.
    pub inviter_addr: EndpointAddr,
    /// The one-time linking secret, pending on the inviting runtime.
    pub secret: [u8; 32],
    /// The identity the new device is linking into.
    pub identity: PdnId,
}

/// `link` was handed a linking payload whose format version this runtime
/// does not speak; refused before dialing. Downcast from the
/// `anyhow::Error` of the identity service's `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported linking payload version: {version}")]
pub struct UnsupportedLinkingVersion {
    /// The version the payload carried.
    pub version: u8,
}

/// The linking dialogue reached the inviting device and ended without an
/// answer — a refusal, distinct from never reaching the inviter
/// ([`InviterUnreachable`](crate::pairing::InviterUnreachable), whose
/// failure precedes this point), from the dialogue outliving the caller's
/// budget ([`DialogueTimeout`]), and from the catch-up timeout after a
/// completed dialogue ([`data_layer::CatchUpTimeout`]). Deliberately
/// reasonless: which of wrong, expired, or already burned applied is
/// uniform toward the dialer by design, and a connection that dies once
/// the request is away — or a handler that fails internally — surfaces
/// exactly the same way; a death before the request is away fails the send
/// instead, unmarked. Downcast from the `anyhow::Error` of the identity
/// service's `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("linking refused by the inviter")]
pub struct LinkingRefused;

/// The linking dialogue did not complete within the caller's budget: the
/// exchange was still in flight — dialing, or a dialed inviter that never
/// answered — when the budget ran out. Distinct from [`LinkingRefused`]
/// (the inviter was reached and the dialogue *ended*) and from
/// [`data_layer::CatchUpTimeout`] (the dialogue completed, and the
/// catch-up spent the rest of the budget). Without this bound a hung
/// inviter would hold the caller for the transport's idle timeout, however
/// small a budget the caller named. Downcast from the `anyhow::Error` of
/// the identity service's `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("linking dialogue did not complete within the caller's budget")]
pub struct DialogueTimeout;

/// `link` was refused before dialing: the identity is already hosted on
/// this runtime. Downcast from the `anyhow::Error` of the identity
/// service's `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("identity already hosted on this runtime: {identity}")]
pub struct IdentityAlreadyHosted {
    /// The identity `link` was attempted for.
    pub identity: PdnId,
}

/// `link` was refused before dialing: another `link` toward the same
/// identity is already in flight on this runtime — the pre-dial guard
/// against two concurrent attempts committing the same not-yet-hosted
/// identity twice. Downcast from the `anyhow::Error` of the identity
/// service's `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("a link toward identity {identity} is already in flight on this runtime")]
pub struct LinkingInProgress {
    /// The identity a concurrent `link` is already attempting.
    pub identity: PdnId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkingLocalFailure {
    PendingDeviceWrite { identity: PdnId, newcomer: NodeId },
    DirectoryTicketMint { identity: PdnId, newcomer: NodeId },
    DataTicketMint { identity: PdnId, newcomer: NodeId },
}

/// The new device's half of the dialogue: the format version and the
/// secret — nothing else. In particular no node id: the inviter takes the
/// newcomer's id from the connection's authenticated peer identity.
#[derive(Debug, Serialize, Deserialize)]
struct LinkingRequest {
    version: u8,
    secret: [u8; 32],
}

/// The inviter's half, sent only after the verify-and-burn and the
/// registration: write tickets to the identity's directory and to its data
/// namespace, both minted fresh from replicas the inviting device hosts
/// locally — the ceremony reads nothing through directory ticket entries,
/// so no payload wait sits in the critical path.
#[derive(Debug, Serialize, Deserialize)]
struct LinkingResponse {
    directory: DocTicket,
    data: DocTicket,
}

/// A generous ceiling on concurrent linkings, never meant to bound anything
/// in practice — it exists only so `shutdown`'s `acquire_many` has a fixed
/// permit count to wait for all of.
const MAX_CONCURRENT_LINKINGS: usize = 1_048_576;

/// How long `LinkingHandler::shutdown` waits for in-flight `accept` calls
/// to release their permit before giving up and letting the router close
/// the endpoint anyway.
const SHUTDOWN_LINKING_BUDGET: Duration = Duration::from_secs(10);

/// The accept side of the linking dialogue, registered at `Runtime::spawn`
/// through the data-layer assembly slot, next to pairing's handler.
#[derive(Debug, Clone)]
pub(crate) struct LinkingHandler {
    state: StateSlot,
    /// One permit held for the duration of each `accept` call; `shutdown`
    /// waits for every permit to come back, bounding shutdown's wait on an
    /// in-flight linking instead of the router aborting it outright.
    in_flight: Arc<tokio::sync::Semaphore>,
}

impl LinkingHandler {
    /// A handler with an unfilled state slot; [`Runtime::spawn`] fills the
    /// slot right after the node comes up.
    ///
    /// [`Runtime::spawn`]: crate::Runtime::spawn
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LINKINGS)),
        }
    }

    /// The slot to fill with the spawned runtime's state.
    pub(crate) fn slot(&self) -> StateSlot {
        Arc::clone(&self.state)
    }

    /// Run the inviter's side of one linking. `None` is a refusal — any
    /// reason at all — answered by the caller with the one uniform close.
    /// `Some(())` means the dialogue completed and the response was sent.
    async fn serve(&self, connection: &Connection) -> Option<()> {
        let (mut send, mut recv) = connection.accept_bi().await.ok()?;
        // The request is read before the lock is taken — pairing's
        // discipline, kept: no network wait ever runs under the lock.
        let request: LinkingRequest = read_message(&mut recv).await.ok()?;
        if request.version != LINKING_FORMAT_VERSION {
            return None;
        }
        // The newcomer's node id, from the connection's authenticated peer
        // identity (the QUIC endpoint key). The request carries no id
        // field, so a registration for a spoofed third-party device is
        // unrepresentable.
        let newcomer = NodeId::from_bytes(*connection.remote_id().as_bytes());

        // The runtime state is held only for the local burn-register-mint,
        // inside this block: both the guard and the strong `Arc` drop at
        // its end, before the network reply below — the same shutdown
        // guarantee as pairing's accept (see `PairingHandler::serve`). No
        // wait on delivery to another device runs under it, ever: the
        // registration below is a local write.
        let response = {
            // The late-bound slot: unfilled (no invite can exist yet) or a
            // runtime already gone both refuse.
            let state = self.state.get()?.upgrade()?;
            let mut state = state.lock().await;

            // The atomic verify-and-burn, before any state change. A wrong
            // secret burns nothing; everything below only runs for a live,
            // unburned secret.
            let identity = state
                .pending_linking_invites
                .verify_and_burn(&request.secret, Instant::now())?;

            // The registration: the newcomer enters this device's own
            // replica as pending — a local write on a device that already
            // holds the directory, so no cross-node delivery sits in the
            // linking critical path. Pending confers nothing: the reply
            // below may never arrive, and a device registered without its
            // tickets must not be served as one. The newcomer confirms
            // itself once it holds them.
            #[cfg(feature = "test-util")]
            let inject_pending_write_failure = if state.fail_next_pending_device_write {
                state.fail_next_pending_device_write = false;
                true
            } else {
                false
            };
            let directory = &state.hosted(identity).ok()?.directory;
            #[cfg(feature = "test-util")]
            let pending_write = if inject_pending_write_failure {
                Err(anyhow::anyhow!("injected pending-device storage failure"))
            } else {
                directory.add_pending_device(newcomer).await
            };
            #[cfg(not(feature = "test-util"))]
            let pending_write = directory.add_pending_device(newcomer).await;
            if let Err(err) = pending_write {
                let _ = state
                    .linking_failures
                    .send(LinkingLocalFailure::PendingDeviceWrite { identity, newcomer });
                tracing::error!(%identity, %newcomer, "linking failed after invite burn while recording the pending device: {err:#}");
                return None;
            }

            // Both bootstrap tickets, minted fresh from local replicas:
            // every device that can mint an invite hosts both — the first
            // device by creation, every further one by its own linking
            // reply.
            let directory_ticket = match directory
                .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
                .await
            {
                Ok(ticket) => ticket,
                Err(err) => {
                    let _ = state
                        .linking_failures
                        .send(LinkingLocalFailure::DirectoryTicketMint { identity, newcomer });
                    tracing::error!(%identity, %newcomer, "linking failed after invite burn while minting the directory ticket: {err:#}");
                    return None;
                }
            };
            let data = match state
                .node
                .share_ticket(
                    identity,
                    ShareMode::Write,
                    AddrInfoOptions::RelayAndAddresses,
                )
                .await
            {
                Ok(ticket) => ticket,
                Err(err) => {
                    let _ = state
                        .linking_failures
                        .send(LinkingLocalFailure::DataTicketMint { identity, newcomer });
                    tracing::error!(%identity, %newcomer, "linking failed after invite burn while minting the data ticket: {err:#}");
                    return None;
                }
            };
            LinkingResponse {
                directory: directory_ticket,
                data,
            }
        };

        // Registration precedes the reply: a response lost from here on
        // leaves the newcomer pending — visible to the identity's other
        // devices, admitted nowhere — and a fresh invite converges (the
        // lost-reply posture). What the newcomer cannot prove it received,
        // this side does not grant.
        write_message(&mut send, &response).await.ok()?;
        send.finish().ok()?;
        // Hold the connection until the dialer closes it, so the response
        // is not cut off by dropping this side first.
        connection.closed().await;
        Some(())
    }
}

impl ProtocolHandler for LinkingHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // Held for the whole call: `shutdown` waits for this permit to
        // return before it does, so a linking in flight when shutdown
        // starts bounds its wait rather than being aborted outright.
        // `acquire` only errs once the semaphore is closed, which this
        // handler never does — the `let else` is exhaustiveness, not a
        // reachable refusal.
        let Ok(_permit) = self.in_flight.acquire().await else {
            return Ok(());
        };
        if self.serve(&connection).await.is_none() {
            // The one uniform refusal, whatever the reason: close without a
            // distinguishing answer, so a prober cannot separate wrong from
            // expired from already-burned — or any other cause.
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }

    /// Waits, bounded by [`SHUTDOWN_LINKING_BUDGET`], for every in-flight
    /// `accept` to finish and release its permit — event-based, no
    /// polling. `Router::shutdown` (iroh) stops dispatching new `accept`
    /// calls before awaiting this, so no new acquisition can race the wait
    /// once it starts — the same reasoning as pairing's handler.
    async fn shutdown(&self) {
        let permits = u32::try_from(MAX_CONCURRENT_LINKINGS).unwrap_or(u32::MAX);
        let _ = tokio::time::timeout(
            SHUTDOWN_LINKING_BUDGET,
            self.in_flight.acquire_many(permits),
        )
        .await;
    }
}

/// The new device's side of one linking: dial the payload's address on the
/// linking ALPN, run the exchange, import the bootstrap tickets from the
/// reply, and return caught up.
///
/// The runtime lock is taken *per phase* and never held across the network
/// round-trip or the catch-up wait — so a link in flight blocks no other
/// service call, and the accept side of this runtime's own ceremonies can
/// take the lock to answer. The caller has already refused an unsupported
/// payload version; the already-hosted refusal here precedes the dial.
///
/// The directory import is followed at once — before the data-namespace
/// import — by arming the identity for session classification, so the
/// data binding never exists ahead of the book that judges its sessions.
/// On any failure after the import, [`undo_link`] rolls back everything
/// this link did — the data-namespace import, the arming, the directory —
/// so a failed link leaves no residue and the identity is unknown to this
/// runtime again.
pub(crate) async fn link_via_dialogue(
    state: &Arc<Mutex<State>>,
    payload: &LinkingPayload,
    timeout: Duration,
) -> Result<()> {
    // A brief lock for the hosted check, the in-flight reservation, and the
    // dial handle (a cheap snapshot); released before any network I/O.
    let (dial, cleanup_tasks) = {
        let mut state_guard = state.lock().await;
        if state_guard.identities.contains_key(&payload.identity) {
            return Err(IdentityAlreadyHosted {
                identity: payload.identity,
            }
            .into());
        }
        if !state_guard.linking_in_flight.insert(payload.identity) {
            return Err(LinkingInProgress {
                identity: payload.identity,
            }
            .into());
        }
        (
            state_guard.node.dial_handle(),
            state_guard.cleanup_tasks.clone(),
        )
    };
    // Reserved for the whole ceremony from here. Released synchronously
    // right after `run` below settles, on every one of its outcomes — a
    // caller retrying immediately after a completed attempt must never
    // race a still-pending detached release. Only a genuine cancellation of
    // *this* function skips that release, and `reservation`'s own `Drop`
    // (spawned detached, since `Drop` is synchronous) is exactly the
    // fallback for that case.
    let reservation =
        LinkingReservation::new(Arc::clone(state), payload.identity, cleanup_tasks.clone());
    let result = link_via_dialogue_inner(
        state,
        payload,
        timeout,
        dial,
        Arc::clone(&reservation.rollback_owns_cleanup),
        cleanup_tasks,
    )
    .await;
    reservation.release().await;
    result
}

/// The body of [`link_via_dialogue`] from the dial onward, factored out so
/// the reservation wrapping it releases synchronously around every exit —
/// the `?` early returns included — without threading the release through
/// each one by hand.
async fn link_via_dialogue_inner(
    state: &Arc<Mutex<State>>,
    payload: &LinkingPayload,
    timeout: Duration,
    dial: data_layer::DialHandle,
    rollback_owns_cleanup: Arc<AtomicBool>,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
) -> Result<()> {
    // The budget covers the whole ceremony from here: the dialogue spends
    // from it first, the catch-up below gets what remains.
    let deadline = tokio::time::Instant::now() + timeout;

    // Network — no lock held, and nothing local minted yet, so a failure
    // anywhere up to the reply rolls back nothing. Bounded by the caller's
    // budget: a hung inviter — dialed, but never answering — surfaces as
    // the typed dialogue timeout instead of holding the caller for the
    // transport's idle timeout.
    let response =
        match tokio::time::timeout_at(deadline, run_linking_dialogue(&dial, payload)).await {
            Ok(response) => response?,
            Err(_budget_spent) => return Err(DialogueTimeout.into()),
        };

    // Import both replicas under a brief lock — local acts, no network
    // wait. Sessions the imports start count for the catch-up below: they
    // start after this instant. The lock is dropped before every call to
    // `undo_link` below: `undo_link` locks `state` itself (so a detached
    // `Drop`-driven rollback can call it too), and a caller still holding
    // the guard while it re-locks would deadlock.
    let before_import = SystemTime::now();
    let rollback_state = Arc::clone(state);
    let mut rollback;
    let directory = {
        let state_guard = state.lock().await;
        // The inviter's address is a live first-sync contact for the
        // imported directory, exactly as a peer's address is in
        // establishment's assembly.
        let mut directory_ticket = response.directory;
        directory_ticket.nodes.push(payload.inviter_addr.clone());
        let directory = PrivateMetadataStore::import(&state_guard.node, directory_ticket).await?;
        rollback = LinkRollbackGuard::new(
            rollback_state,
            payload.identity,
            directory.namespace(),
            rollback_owns_cleanup,
            cleanup_tasks,
        );
        // Armed before the data namespace exists: the moment the import
        // below registers the binding, sessions are judged through this
        // directory — a still-catching-up book refuses callers it cannot
        // resolve (fail-closed, re-served a reconcile pass after their
        // device records land), and no serving window opens on the
        // long-lived namespace id.
        if let Err(err) = state_guard.node.host_identity(payload.identity, &directory) {
            drop(state_guard);
            undo_link(state, payload.identity, directory.namespace(), None).await;
            rollback.disarm();
            return Err(err);
        }
        let data_import = state_guard
            .node
            .import_namespace(payload.identity, response.data)
            .await;
        match data_import {
            Ok(data_import) => rollback.set_data_import(data_import),
            Err(err) => {
                // The rollback begins with the import: a directory whose
                // sibling import failed must not survive it.
                drop(state_guard);
                undo_link(state, payload.identity, directory.namespace(), None).await;
                rollback.disarm();
                return Err(err);
            }
        }
        directory
    };

    #[cfg(feature = "test-util")]
    if let Some(pause) = state.lock().await.link_after_import_pause.take() {
        pause.reached.notify_one();
        pause.release.notified().await;
    }

    // The rest of the budget bounds the catch-up, against a peer that
    // answered the dialogue moments ago — no lock held. Beyond it, the
    // node's periodic reconcile pass keeps the replicas converging. No lock
    // is held here either, so a cancellation in this stretch is exactly
    // what `rollback`'s `Drop` exists for — the explicit branch below only
    // covers a *completed* wait that came back `Err`.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if let Err(err) = directory.wait_caught_up(before_import, remaining).await {
        rollback.roll_back().await;
        return Err(err).context("the imported directory did not catch up in time");
    }
    if let Err(err) = directory.cleanup_pending_devices().await {
        rollback.roll_back().await;
        return Err(err).context("pending-device cleanup failed after import");
    }

    // The armer's subscription, taken before the handle moves into the
    // hosted set: pairs established on the identity's other devices —
    // already replicated or arriving later — register here as their records
    // and ticket payloads land, so this device serves and is servable
    // without ever touching the grant surface itself. Taken before the
    // confirmation below, so that confirmation is the last thing that can
    // fail: a rollback that ran after it would forget the directory here
    // while the record it wrote had already replicated to the inviter.
    let changes = match directory.changes().await {
        Ok(changes) => changes,
        Err(err) => {
            rollback.roll_back().await;
            return Err(err);
        }
    };

    // The tickets are in hand and the directory has caught up, so this
    // device — its id read from the dial's own authenticated identity, the
    // one the inviter registered as pending — converts that registration
    // into a confirmed one. Only a holder of the directory's write ticket
    // can write this record, which is why the newcomer writes it: the
    // inviter, having sent the reply into a connection that may drop,
    // cannot establish that it arrived. Until it replicates, the identity's
    // other devices refuse this one's sessions and re-serve it later.
    let own_device = NodeId::from_bytes(*dial.id().as_bytes());
    if let Err(err) = directory.confirm_device(own_device).await {
        rollback.roll_back().await;
        return Err(err).context("the linked device could not confirm itself in the directory");
    }

    // Success: the identity joins the runtime's hosted set. Classification
    // was armed before the imports (above), and by now the caught-up
    // directory carries the identity's device records, this device's own
    // confirmation included. Nothing below can fail.
    let mut guard = state.lock().await;
    guard
        .identities
        .insert(payload.identity, HostedIdentity { directory });
    crate::connections::spawn_connection_armer(Arc::downgrade(state), payload.identity, changes);
    rollback.disarm();
    Ok(())
}

/// Reserves an identity against a concurrent `link` while this one is in
/// flight, releasing the reservation on every exit path of
/// [`link_via_dialogue`] — success, every explicit failure, and
/// cancellation alike — the linking analog of `grant_binders`'
/// insert-before-spawn/remove-on-exit pattern (`connections.rs`). Held for
/// the whole function, unlike [`LinkRollbackGuard`] (constructed only once
/// the directory import succeeds): a cancellation during the dial or the
/// round trip, before any rollback guard exists, must still release this
/// one.
struct LinkingReservation {
    state: Arc<Mutex<State>>,
    identity: PdnId,
    rollback_owns_cleanup: Arc<AtomicBool>,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
    armed: bool,
}

impl LinkingReservation {
    fn new(
        state: Arc<Mutex<State>>,
        identity: PdnId,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            state,
            identity,
            rollback_owns_cleanup: Arc::new(AtomicBool::new(false)),
            cleanup_tasks,
            armed: true,
        }
    }

    /// Release the reservation now, synchronously — the normal path for
    /// every outcome of [`link_via_dialogue_inner`]. A caller retrying
    /// immediately after this one returns must see the identity free, not
    /// race a detached release that has not run yet.
    async fn release(mut self) {
        self.state
            .lock()
            .await
            .linking_in_flight
            .remove(&self.identity);
        self.armed = false;
    }
}

impl Drop for LinkingReservation {
    /// Only fires on genuine cancellation — the future driving
    /// [`link_via_dialogue`] dropped before [`Self::release`] ran. `Drop`
    /// is synchronous and the removal is not, so this spawns a detached
    /// task; unlike the normal path, a cancelled attempt leaves no caller
    /// waiting to retry immediately, so a slight delay here is harmless.
    fn drop(&mut self) {
        if !self.armed || self.rollback_owns_cleanup.load(Ordering::Acquire) {
            return;
        }
        let state = Arc::clone(&self.state);
        let identity = self.identity;
        self.cleanup_tasks.spawn(async move {
            state.lock().await.linking_in_flight.remove(&identity);
        });
    }
}

/// Rolls back a link's local effects if the future driving [`link_via_dialogue`]
/// is dropped before it disarms — a caller giving up or a task aborted skips
/// every explicit `if let Err`/success branch above, so this is the only
/// thing that runs [`undo_link`] in that case. `Drop` is synchronous and the
/// rollback is not, so a still-armed guard spawns a detached task to run it
/// — a new, explicit dependency on an ambient Tokio runtime this ceremony
/// did not need before.
struct LinkRollbackGuard {
    state: Arc<Mutex<State>>,
    identity: PdnId,
    directory_namespace: NamespaceId,
    data_import: Option<SelfCleaningImport>,
    owns_reservation_cleanup: Arc<AtomicBool>,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
    armed: bool,
}

impl LinkRollbackGuard {
    fn new(
        state: Arc<Mutex<State>>,
        identity: PdnId,
        directory_namespace: NamespaceId,
        owns_reservation_cleanup: Arc<AtomicBool>,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        owns_reservation_cleanup.store(true, Ordering::Release);
        Self {
            state,
            identity,
            directory_namespace,
            data_import: None,
            owns_reservation_cleanup,
            cleanup_tasks,
            armed: true,
        }
    }

    /// Record that the data-namespace import has succeeded, so a rollback
    /// from here on undoes it too.
    fn set_data_import(&mut self, data_import: NamespaceImport) {
        self.data_import = Some(SelfCleaningImport::new(
            Arc::clone(&self.state),
            data_import,
            self.cleanup_tasks.clone(),
        ));
    }

    /// Hand the data import to a caller running its own synchronous
    /// rollback — the guard does not retain it afterward.
    fn take_data_import(&mut self) -> Option<SelfCleaningImport> {
        self.data_import.take()
    }

    /// Undo everything the link did, then disarm — the tail every failure
    /// after the imports shares. The undo consumes the data import, so what
    /// is left for [`disarm`](Self::disarm) to commit is nothing.
    async fn roll_back(&mut self) {
        let data_import = self.take_data_import();
        undo_link(
            &self.state,
            self.identity,
            self.directory_namespace,
            data_import,
        )
        .await;
        self.disarm();
    }

    /// The ceremony reached a normal return — commit or an error branch
    /// with its own already-`await`ed cleanup — so the drop below must do
    /// nothing. Commits any still-held `data_import` too: implementing
    /// `Drop` on the guard does not stop its fields from being dropped in
    /// turn once the guard itself is — a `SelfCleaningImport` left in
    /// `Some` here would still self-undo on the success path, moments
    /// after `link_via_dialogue` reported it caught up.
    fn disarm(&mut self) {
        self.armed = false;
        self.owns_reservation_cleanup
            .store(false, Ordering::Release);
        if let Some(import) = self.data_import.take() {
            import.commit();
        }
    }
}

impl Drop for LinkRollbackGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = Arc::clone(&self.state);
        let identity = self.identity;
        let directory_namespace = self.directory_namespace;
        let data_import = self.data_import.take();
        let owns_reservation_cleanup = Arc::clone(&self.owns_reservation_cleanup);
        self.cleanup_tasks.spawn(async move {
            undo_link(&state, identity, directory_namespace, data_import).await;
            state.lock().await.linking_in_flight.remove(&identity);
            owns_reservation_cleanup.store(false, Ordering::Release);
        });
    }
}

/// A `NamespaceImport` that undoes itself if dropped before [`Self::undo`]
/// runs — the obligation to undo an import must travel with the value
/// itself, not only with whichever guard minted it: moving the bare import
/// into a local ahead of an `.await` (as the explicit rollback branches in
/// [`link_via_dialogue`] do, via [`LinkRollbackGuard::take_data_import`])
/// loses the obligation the instant that local's own future is dropped
/// mid-`.await` — exactly the race [`LinkRollbackGuard`]'s own `Drop`
/// cannot see, since the field it would check is already empty by then.
struct SelfCleaningImport {
    state: Arc<Mutex<State>>,
    import: Option<NamespaceImport>,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
}

impl SelfCleaningImport {
    fn new(
        state: Arc<Mutex<State>>,
        import: NamespaceImport,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            state,
            import: Some(import),
            cleanup_tasks,
        }
    }

    /// Undo now, on a spawned task, and wait for it: `tokio::spawn` hands
    /// the future to the runtime with no `.await` gap of its own, so the
    /// undo keeps running to completion even if whatever awaits this method
    /// is itself dropped mid-await. Awaiting the spawned task's handle
    /// makes this mean "undone" for a caller that runs to completion, the
    /// same as a direct `.await` would.
    async fn undo(mut self) {
        let Some(import) = self.import.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let handle = self.cleanup_tasks.spawn(async move {
            let state = state.lock().await;
            let _ = state.node.undo_import_namespace(import).await;
        });
        let _ = handle.await;
    }

    /// The import succeeded and is now owned by the runtime's registry:
    /// disable self-cleaning without running it. Implementing `Drop` does
    /// not stop a value's fields from being dropped once it is — clearing
    /// `import` here is what makes the coming `Drop` a no-op, not the fact
    /// that this method was called at all.
    fn commit(mut self) {
        self.import = None;
    }
}

impl Drop for SelfCleaningImport {
    /// Only fires when `undo` was never called at all — cancellation before
    /// this value's own cleanup ever started. Belt-and-suspenders for any
    /// future refactor that holds the value across some other `.await`
    /// first; today every caller of `take_data_import` calls `undo`
    /// immediately.
    fn drop(&mut self) {
        let Some(import) = self.import.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        self.cleanup_tasks.spawn(async move {
            let state = state.lock().await;
            let _ = state.node.undo_import_namespace(import).await;
        });
    }
}

/// The network half of `link`: dial the inviter on the linking ALPN,
/// present the secret, read the bootstrap reply. No local state is touched,
/// so a failure anywhere here rolls back nothing.
async fn run_linking_dialogue(
    dial: &data_layer::DialHandle,
    payload: &LinkingPayload,
) -> Result<LinkingResponse> {
    let connection = dial
        .connect(payload.inviter_addr.clone(), LINKING_ALPN)
        .await
        .context(crate::pairing::InviterUnreachable)?;
    let response: LinkingResponse = async {
        let (mut send, mut recv) = connection.open_bi().await?;
        write_message(
            &mut send,
            &LinkingRequest {
                version: LINKING_FORMAT_VERSION,
                secret: payload.secret,
            },
        )
        .await?;
        send.finish()?;
        // Refusals are uniform by design: the connection just closes, and
        // this read fails without saying why — the typed marker says only
        // that the inviter was reached and no answer came.
        read_message(&mut recv).await.context(LinkingRefused)
    }
    .await?;
    connection.close(0u32.into(), b"done");
    Ok(response)
}

/// Undo an abandoned link's local effects, in reverse order of their
/// creation: the data-namespace import (when it happened), the
/// classification arming, and the directory import — so the dialing node
/// keeps no residue and the identity's operations refuse as unknown again,
/// not as storage errors against a dropped replica. Best-effort on every
/// step: the link already failed, and the original error is what the
/// caller reports.
///
/// The data namespace is undone rather than forgotten outright: this
/// runtime may already have been bound to that issuer before the link — a
/// peer's granted namespace registers under the issuer too, and the
/// pre-dial guard cannot see it (it reads the hosted set, not the node's
/// issuer registry). Forgetting by issuer would delete a replica this link
/// never imported, permanently: a rollback must restore what it displaced,
/// never destroy state that predates it.
///
/// Takes the state `Arc` rather than an already-held guard, and locks only
/// after the import is undone: [`SelfCleaningImport::undo`] locks `state`
/// itself to survive its own cancellation, so a caller passing in a guard
/// held across this call would deadlock against it.
async fn undo_link(
    state: &Arc<Mutex<State>>,
    identity: PdnId,
    directory_namespace: NamespaceId,
    data_import: Option<SelfCleaningImport>,
) {
    if let Some(import) = data_import {
        import.undo().await;
    }
    let state = state.lock().await;
    let _ = state.node.unhost_identity(identity);
    let _ = state.node.forget_doc(directory_namespace).await;
}

// `tracked_doc_count` is behind `test-util`; this module's one test needs
// it, so the module does too.
#[cfg(all(test, feature = "test-util"))]
mod tests {
    use data_layer::SyncNode;
    use test_utils::ids;

    use super::*;
    use crate::runtime::Runtime;

    /// `SelfCleaningImport` undoes its import when dropped without
    /// [`SelfCleaningImport::undo`] ever being called — the one-instruction
    /// race `LinkRollbackGuard`'s own `Drop` cannot see, since the field it
    /// would check is already empty by the time ownership has moved out via
    /// `take_data_import`. Deterministic, unlike the integration-level
    /// cancellation sweep in `tests/linking.rs`, which cannot reliably land
    /// on this exact race.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_without_undo_still_undoes_the_import() -> Result<()> {
        let rt = Runtime::spawn().await?;
        let state = Arc::clone(&rt.state);

        let scratch = SyncNode::spawn().await?;
        scratch.create_namespace(ids::DAVE).await?;
        let ticket = scratch
            .share_ticket(
                ids::DAVE,
                ShareMode::Write,
                AddrInfoOptions::RelayAndAddresses,
            )
            .await?;

        let (import, before) = {
            let guard = state.lock().await;
            let import = guard.node.import_namespace(ids::DAVE, ticket).await?;
            let before = guard.node.tracked_doc_count()?;
            (import, before)
        };

        let cleanup_tasks = state.lock().await.cleanup_tasks.clone();
        drop(SelfCleaningImport::new(
            Arc::clone(&state),
            import,
            cleanup_tasks,
        ));

        let settled = test_utils::eventually(|| async {
            let guard = state.lock().await;
            Ok(guard.node.tracked_doc_count()? < before)
        })
        .await?;
        assert!(
            settled,
            "dropping SelfCleaningImport without calling undo() must still undo the import"
        );

        rt.shutdown().await?;
        scratch.shutdown().await?;
        Ok(())
    }
}
