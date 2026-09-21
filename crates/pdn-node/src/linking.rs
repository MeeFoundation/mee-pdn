//! The device-linking protocol (ADR-0012): establishment's shape on a
//! dedicated ALPN — separate from pairing's because the stakes differ (a
//! whole-directory write ticket versus per-connection read tickets), so the
//! two wire formats evolve independently. The inviter verifies-and-burns
//! before any state change, registers the newcomer as pending under the
//! connection's authenticated node id, and replies with fresh write tickets
//! to the directory and the data namespace. Pending confers nothing; the
//! newcomer confirms itself once the tickets are in hand, since only a
//! holder of the directory's write ticket can, which is evidence the reply
//! arrived. The dial side arms classification the moment the directory is
//! imported — before the data namespace exists, so no serving window opens
//! on the long-lived namespace id — and rolls everything back on any
//! failure after the import.

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

pub(crate) const LINKING_ALPN: &[u8] = b"/pdn/linking/0";

/// Any other version is refused before dialing by the dialer, and
/// uniformly by the inviter.
pub const LINKING_FORMAT_VERSION: u8 = 0;

/// The linking payload: bearer-free on purpose, since it is shown on a
/// screen and photographable — the bootstrap tickets ride the encrypted
/// reply. Its string/QR encoding is a host concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkingPayload {
    pub version: u8,
    /// Where the new device dials.
    pub inviter_addr: EndpointAddr,
    pub secret: [u8; 32],
    pub identity: PdnId,
}

/// Refused before dialing. Downcast from the `anyhow::Error` of `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unsupported linking payload version: {version}")]
pub struct UnsupportedLinkingVersion {
    pub version: u8,
}

/// The dialogue reached the inviting device and ended without an answer.
/// Reasonless by design; a connection that dies once the request is away,
/// or a handler that fails internally, surfaces the same way. Distinct from
/// [`InviterUnreachable`](crate::pairing::InviterUnreachable), from
/// [`DialogueTimeout`], and from [`data_layer::CatchUpTimeout`]. Downcast
/// from the `anyhow::Error` of `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("linking refused by the inviter")]
pub struct LinkingRefused;

/// The exchange was still in flight when the caller's budget ran out —
/// distinct from [`LinkingRefused`] (the dialogue ended) and from
/// [`data_layer::CatchUpTimeout`] (the dialogue completed). Without this
/// bound a hung inviter holds the caller for the transport's idle timeout.
/// Downcast from the `anyhow::Error` of `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("linking dialogue did not complete within the caller's budget")]
pub struct DialogueTimeout;

/// Refused before dialing. Downcast from the `anyhow::Error` of `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("identity already hosted on this runtime: {identity}")]
pub struct IdentityAlreadyHosted {
    pub identity: PdnId,
}

/// Another `link` toward the same identity is in flight; refused before
/// dialing. Downcast from the `anyhow::Error` of `link`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("a link toward identity {identity} is already in flight on this runtime")]
pub struct LinkingInProgress {
    pub identity: PdnId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkingLocalFailure {
    PendingDeviceWrite { identity: PdnId, newcomer: NodeId },
    DirectoryTicketMint { identity: PdnId, newcomer: NodeId },
    DataTicketMint { identity: PdnId, newcomer: NodeId },
}

/// No node id: the inviter takes the newcomer's from the connection's
/// authenticated peer identity, so a spoofed registration is
/// unrepresentable.
#[derive(Debug, Serialize, Deserialize)]
struct LinkingRequest {
    version: u8,
    secret: [u8; 32],
}

/// Both minted fresh from local replicas: the ceremony reads nothing through
/// directory ticket entries, so no payload wait sits in the critical path.
#[derive(Debug, Serialize, Deserialize)]
struct LinkingResponse {
    directory: DocTicket,
    data: DocTicket,
}

/// Never meant to bound anything: exists so `shutdown`'s `acquire_many` has
/// a fixed permit count to wait for.
const MAX_CONCURRENT_LINKINGS: usize = 1_048_576;

const SHUTDOWN_LINKING_BUDGET: Duration = Duration::from_secs(10);

/// The accept side of the linking dialogue.
#[derive(Debug, Clone)]
pub(crate) struct LinkingHandler {
    state: StateSlot,
    /// One permit per `accept` in flight; see `PairingHandler`.
    in_flight: Arc<tokio::sync::Semaphore>,
}

impl LinkingHandler {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LINKINGS)),
        }
    }

    pub(crate) fn slot(&self) -> StateSlot {
        Arc::clone(&self.state)
    }

    /// `None` is a refusal, any reason at all, answered by the caller with
    /// the one uniform close.
    async fn serve(&self, connection: &Connection) -> Option<()> {
        let (mut send, mut recv) = connection.accept_bi().await.ok()?;
        // Read before the lock: no network wait runs under it.
        let request: LinkingRequest = read_message(&mut recv).await.ok()?;
        if request.version != LINKING_FORMAT_VERSION {
            return None;
        }
        let newcomer = NodeId::from_bytes(*connection.remote_id().as_bytes());

        // The state is held only for the local burn-register-mint, dropped
        // before the network reply. The registration is a local write, so
        // no cross-node delivery sits in the critical path.
        let response = {
            let state = self.state.get()?.upgrade()?;
            let mut state = state.lock().await;

            // Before any state change.
            let identity = state
                .pending_linking_invites
                .verify_and_burn(&request.secret, Instant::now())?;

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

            // Every device that can mint an invite hosts both replicas —
            // the first by creation, every further one by its own reply.
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

        // Registration precedes the reply: a lost response leaves the
        // newcomer pending, and a fresh invite converges.
        write_message(&mut send, &response).await.ok()?;
        send.finish().ok()?;
        // Held until the dialer closes, so the response is not cut off.
        connection.closed().await;
        Some(())
    }
}

impl ProtocolHandler for LinkingHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The `let else` is exhaustiveness: the semaphore is never closed.
        let Ok(_permit) = self.in_flight.acquire().await else {
            return Ok(());
        };
        if self.serve(&connection).await.is_none() {
            // The one uniform refusal.
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }

    /// See `PairingHandler::shutdown`.
    async fn shutdown(&self) {
        let permits = u32::try_from(MAX_CONCURRENT_LINKINGS).unwrap_or(u32::MAX);
        let _ = tokio::time::timeout(
            SHUTDOWN_LINKING_BUDGET,
            self.in_flight.acquire_many(permits),
        )
        .await;
    }
}

/// The new device's side. The runtime lock is taken per phase and never
/// held across the round-trip or the catch-up wait, so the accept side of
/// this runtime's own ceremonies can take it to answer.
pub(crate) async fn link_via_dialogue(
    state: &Arc<Mutex<State>>,
    payload: &LinkingPayload,
    timeout: Duration,
) -> Result<()> {
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
    // Released synchronously on every outcome, so a caller retrying at once
    // never races a detached release; the reservation's `Drop` covers a
    // cancellation of this function alone.
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

/// Factored out so the reservation releases synchronously around every
/// exit, `?` early returns included.
#[allow(clippy::too_many_lines)] // one ceremony, each rollback branch beside the step it guards
async fn link_via_dialogue_inner(
    state: &Arc<Mutex<State>>,
    payload: &LinkingPayload,
    timeout: Duration,
    dial: data_layer::DialHandle,
    rollback_owns_cleanup: Arc<AtomicBool>,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
) -> Result<()> {
    // The dialogue spends from the budget first; the catch-up gets the rest.
    let deadline = tokio::time::Instant::now() + timeout;

    // Nothing local minted yet: a failure up to the reply rolls back nothing.
    let response =
        match tokio::time::timeout_at(deadline, run_linking_dialogue(&dial, payload)).await {
            Ok(response) => response?,
            Err(_budget_spent) => return Err(DialogueTimeout.into()),
        };

    // Sessions the imports start count for the catch-up: they start after
    // this instant. The lock is dropped before every `undo_link` call:
    // `undo_link` locks `state` itself, so a detached rollback can call it.
    let before_import = SystemTime::now();
    let rollback_state = Arc::clone(state);
    let mut rollback;
    let directory = {
        let state_guard = state.lock().await;
        let mut directory_ticket = response.directory;
        directory_ticket.nodes.push(payload.inviter_addr.clone());
        // The identity's own half of the node, brought up by this link and
        // dropped whole if it fails (ADR-0013).
        state_guard
            .node
            .provision_identity(payload.identity)
            .await?;
        let directory =
            PrivateMetadataStore::import(&state_guard.node, payload.identity, directory_ticket)
                .await?;
        rollback = LinkRollbackGuard::new(
            rollback_state,
            payload.identity,
            directory.namespace(),
            rollback_owns_cleanup,
            cleanup_tasks,
        );
        // Armed before the data namespace exists: a still-catching-up book
        // refuses callers it cannot resolve, and no serving window opens on
        // the long-lived namespace id.
        if let Err(err) = state_guard.node.host_identity(payload.identity, &directory) {
            drop(state_guard);
            undo_link(state, payload.identity, directory.namespace(), None).await;
            rollback.disarm();
            return Err(err);
        }
        let data_import = state_guard
            .node
            .import_namespace(payload.identity, payload.identity, response.data)
            .await;
        match data_import {
            Ok(data_import) => rollback.set_data_import(data_import),
            Err(err) => {
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

    // No lock held: a cancellation here is what `rollback`'s `Drop` exists
    // for — the explicit branch covers only a completed wait.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if let Err(err) = directory.wait_caught_up(before_import, remaining).await {
        rollback.roll_back().await;
        return Err(err).context("the imported directory did not catch up in time");
    }
    if let Err(err) = directory.cleanup_pending_devices().await {
        rollback.roll_back().await;
        return Err(err).context("pending-device cleanup failed after import");
    }

    // The armer's subscription, taken before the commit point so the commit
    // is the last thing that can fail.
    let changes = match directory.changes().await {
        Ok(changes) => changes,
        Err(err) => {
            rollback.roll_back().await;
            return Err(err);
        }
    };

    #[cfg(feature = "test-util")]
    if let Some(pause) = state.lock().await.link_before_commit_pause.take() {
        pause.reached.notify_one();
        pause.release.notified().await;
    }

    // The commit point: after the catch-up, before anything is written into
    // the directory this device now shares — a failure rolls the link back
    // whole, and no sibling ever saw this attempt.
    let mut guard = state.lock().await;
    if let Err(err) = guard
        .commit_hosting(payload.identity, directory.namespace())
        .await
    {
        drop(guard);
        rollback.roll_back().await;
        return Err(err).context("the hosted-identities record could not be written");
    }
    let author = guard.node.default_author(payload.identity)?;
    guard
        .identities
        .insert(payload.identity, HostedIdentity { directory, author });

    // The confirmation, after the commit point and never before it: written
    // before the commit it would stand on every sibling with no local
    // failure able to take it back. A failure here neither fails the link
    // nor rolls it back — the sweep repeats the write.
    let own_device = NodeId::from_bytes(*dial.id().as_bytes());
    debug_assert_eq!(
        own_device,
        guard.node.node_id(),
        "the dial's authenticated id is this node's own"
    );
    if let Err(err) =
        crate::connections::ensure_own_device_confirmed(&mut guard, payload.identity).await
    {
        tracing::warn!(
            identity = %payload.identity,
            %own_device,
            "the linked device is not confirmed in the directory yet; the sweep repeats it: {err:#}"
        );
    }
    drop(guard);
    crate::connections::spawn_connection_armer(Arc::downgrade(state), payload.identity, changes);
    rollback.disarm();
    Ok(())
}

/// Reserves an identity against a concurrent `link`. Held for the whole
/// function, unlike [`LinkRollbackGuard`]: a cancellation during the dial,
/// before any rollback guard exists, must still release it.
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

    /// The normal path: a caller retrying at once must see the identity
    /// free.
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
    /// Cancellation only; spawned detached since `Drop` is synchronous.
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

/// Rolls back a link's local effects if the linking future is dropped
/// before it disarms — the only thing that runs [`undo_link`] then. `Drop`
/// is synchronous and the rollback is not, so it is spawned detached.
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

    fn set_data_import(&mut self, data_import: NamespaceImport) {
        self.data_import = Some(SelfCleaningImport::new(
            Arc::clone(&self.state),
            data_import,
            self.cleanup_tasks.clone(),
        ));
    }

    fn take_data_import(&mut self) -> Option<SelfCleaningImport> {
        self.data_import.take()
    }

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

    /// Commits any still-held `data_import` too: `Drop` on the guard does
    /// not stop its fields from being dropped in turn, and a
    /// `SelfCleaningImport` left in `Some` would self-undo on the success
    /// path.
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
/// runs: the obligation travels with the value, because a bare import moved
/// into a local ahead of an `.await` would lose it the instant that future
/// is dropped — a race [`LinkRollbackGuard`]'s `Drop` cannot see, its
/// field being empty by then.
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

    /// Spawned and awaited: the undo runs to completion even if the caller
    /// is dropped mid-await, and means "undone" for one that is not.
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

    /// Clearing `import` is what makes the coming `Drop` a no-op.
    fn commit(mut self) {
        self.import = None;
    }
}

impl Drop for SelfCleaningImport {
    /// Fires only when `undo` was never called — a cancellation before the
    /// value's own cleanup started.
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

/// The network half of `link`; touches no local state.
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
        // A refusal is just the connection closing.
        read_message(&mut recv).await.context(LinkingRefused)
    }
    .await?;
    connection.close(0u32.into(), b"done");
    Ok(response)
}

/// Undo an abandoned link's local effects in reverse order, best-effort.
/// The link brought up the identity's own half of the node, so dropping
/// it reaches exactly what the link imported and nothing else: a
/// namespace of the same issuer held under another identity's grant is in
/// that identity's stores and is untouched. Locks `state` only after the
/// import is undone — [`SelfCleaningImport::undo`] locks it itself.
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
    let _ = state.node.forget_doc(identity, directory_namespace).await;
    let _ = state.node.unhost_identity(identity).await;
}

// `tracked_doc_count` is behind `test-util`.
#[cfg(all(test, feature = "test-util"))]
mod tests {
    use data_layer::SyncNode;
    use test_utils::ids;

    use super::*;
    use crate::runtime::Runtime;

    /// The one-instruction race `LinkRollbackGuard`'s `Drop` cannot see,
    /// pinned deterministically; the cancellation sweep in
    /// `tests/linking.rs` cannot reliably land on it.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_without_undo_still_undoes_the_import() -> Result<()> {
        let rt = Runtime::spawn(data_layer::SpawnOptions::memory()).await?;
        let state = Arc::clone(&rt.state);

        let scratch = SyncNode::spawn(data_layer::SpawnOptions::memory()).await?;
        scratch.provision_identity(ids::DAVE).await?;
        scratch.create_namespace(ids::DAVE, ids::DAVE).await?;
        let ticket = scratch
            .share_ticket(
                ids::DAVE,
                ids::DAVE,
                ShareMode::Write,
                AddrInfoOptions::RelayAndAddresses,
            )
            .await?;

        let (import, before) = {
            let guard = state.lock().await;
            guard.node.provision_identity(ids::ALICE).await?;
            let import = guard
                .node
                .import_namespace(ids::ALICE, ids::DAVE, ticket)
                .await?;
            let before = guard.node.tracked_doc_count(ids::ALICE)?;
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
            Ok(guard.node.tracked_doc_count(ids::ALICE)? < before)
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
