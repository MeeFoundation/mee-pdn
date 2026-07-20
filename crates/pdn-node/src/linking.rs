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
//! the newcomer in its own directory replica — the node id taken from the
//! connection's authenticated peer identity, never from a claimed field —
//! and answers with freshly minted write tickets to the directory and the
//! identity's data namespace. Commit precedes the reply: a response lost
//! from there on leaves a registered-but-absent device record (harmless —
//! device records carry no liveness semantics), and a fresh invite
//! converges.
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

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use data_layer::{
    AcceptError, AddrInfoOptions, Connection, DocTicket, EndpointAddr, NamespaceId,
    NamespaceImport, PrivateMetadataStore, ProtocolHandler, ShareMode, SyncNode,
};
use pdn_types::{NodeId, PdnId};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::pairing::{read_message, write_message, StateSlot};
use crate::runtime::{HostedIdentity, State};

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

/// The accept side of the linking dialogue, registered at `Runtime::spawn`
/// through the data-layer assembly slot, next to pairing's handler.
#[derive(Debug, Clone)]
pub(crate) struct LinkingHandler {
    state: StateSlot,
}

impl LinkingHandler {
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

            // The commit: register the newcomer in this device's own
            // replica — a local write on a device that already holds the
            // directory, so no cross-node delivery sits in the linking
            // critical path.
            let directory = &state.hosted(identity).ok()?.directory;
            directory.add_device(newcomer).await.ok()?;

            // Both bootstrap tickets, minted fresh from local replicas:
            // every device that can mint an invite hosts both — the first
            // device by creation, every further one by its own linking
            // reply.
            let directory_ticket = directory
                .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
                .await
                .ok()?;
            let data = state
                .node
                .share_ticket(
                    identity,
                    ShareMode::Write,
                    AddrInfoOptions::RelayAndAddresses,
                )
                .await
                .ok()?;
            LinkingResponse {
                directory: directory_ticket,
                data,
            }
        };

        // Commit precedes the reply: a response lost from here on leaves a
        // registered-but-absent device record, and a fresh invite
        // converges (the lost-reply posture).
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
        if self.serve(&connection).await.is_none() {
            // The one uniform refusal, whatever the reason: close without a
            // distinguishing answer, so a prober cannot separate wrong from
            // expired from already-burned — or any other cause.
            connection.close(0u32.into(), b"");
        }
        Ok(())
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
    // A brief lock for the hosted check and the dial handle (a cheap
    // snapshot); released before any network I/O.
    let dial = {
        let state = state.lock().await;
        if state.identities.contains_key(&payload.identity) {
            bail!(
                "identity already hosted on this runtime: {}",
                payload.identity
            );
        }
        state.node.dial_handle()
    };

    // Network — no lock held, and nothing local minted yet, so a failure
    // anywhere up to the reply rolls back nothing.
    let response = run_linking_dialogue(&dial, payload).await?;

    // Import both replicas under a brief lock — local acts, no network
    // wait. Sessions the imports start count for the catch-up below: they
    // start after this instant.
    let before_import = SystemTime::now();
    let (directory, data_import) = {
        let state = state.lock().await;
        // The inviter's address is a live first-sync contact for the
        // imported directory, exactly as a peer's address is in
        // establishment's assembly.
        let mut directory_ticket = response.directory;
        directory_ticket.nodes.push(payload.inviter_addr.clone());
        let directory = PrivateMetadataStore::import(&state.node, directory_ticket).await?;
        // Armed before the data namespace exists: the moment the import
        // below registers the binding, sessions are judged through this
        // directory — a still-catching-up book refuses callers it cannot
        // resolve (fail-closed, re-served a reconcile pass after their
        // device records land), and no serving window opens on the
        // long-lived namespace id.
        if let Err(err) = state.node.host_identity(payload.identity, &directory) {
            undo_link(&state.node, payload.identity, directory.namespace(), None).await;
            return Err(err);
        }
        match state
            .node
            .import_namespace(payload.identity, response.data)
            .await
        {
            Ok(data_import) => (directory, data_import),
            Err(err) => {
                // The rollback begins with the import: a directory whose
                // sibling import failed must not survive it.
                undo_link(&state.node, payload.identity, directory.namespace(), None).await;
                return Err(err);
            }
        }
    };

    // The one bounded wait, against a peer that answered the dialogue
    // moments ago — no lock held. Beyond it, the node's periodic reconcile
    // pass keeps the replicas converging.
    if let Err(err) = directory.wait_caught_up(before_import, timeout).await {
        let state = state.lock().await;
        undo_link(
            &state.node,
            payload.identity,
            directory.namespace(),
            Some(data_import),
        )
        .await;
        return Err(err).context("the imported directory did not catch up in time");
    }

    // Success: the identity joins the runtime's hosted set. Classification
    // was armed before the imports (above), and by now the caught-up
    // directory carries the identity's device records, this device included
    // (the inviter registered it during the dialogue). The armer's
    // subscription is taken before the handle moves into the hosted set:
    // pairs established on the identity's other devices — already
    // replicated or arriving later — register here as their records and
    // ticket payloads land, so this device serves and is servable without
    // ever touching the grant surface itself.
    let mut guard = state.lock().await;
    let changes = match directory.changes().await {
        Ok(changes) => changes,
        Err(err) => {
            undo_link(
                &guard.node,
                payload.identity,
                directory.namespace(),
                Some(data_import),
            )
            .await;
            return Err(err);
        }
    };
    guard
        .identities
        .insert(payload.identity, HostedIdentity { directory });
    crate::connections::spawn_connection_armer(Arc::downgrade(state), payload.identity, changes);
    Ok(())
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
        .context("could not reach the inviter")?;
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
        // this read fails without saying why.
        read_message(&mut recv)
            .await
            .context("linking refused by the inviter")
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
async fn undo_link(
    node: &SyncNode,
    identity: PdnId,
    directory_namespace: NamespaceId,
    data_import: Option<NamespaceImport>,
) {
    if let Some(import) = data_import {
        let _ = node.undo_import_namespace(import).await;
    }
    let _ = node.unhost_identity(identity);
    let _ = node.forget_doc(directory_namespace).await;
}
