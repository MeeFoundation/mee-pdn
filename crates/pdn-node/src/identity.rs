//! The identity service: create an identity on its first device, link every
//! further device over the linking dialogue.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use data_layer::{AddrInfoOptions, PrivateMetadataStore, ShareMode};
use pdn_types::PdnId;

use crate::{
    linking::{
        link_via_dialogue, LinkingPayload, UnsupportedLinkingVersion, LINKING_FORMAT_VERSION,
    },
    pairing::DEFAULT_INVITE_LIFETIME,
    runtime::{HostedIdentity, Runtime},
};

/// The private-metadata directory kind under which an identity's own
/// data-namespace ticket is published at creation — the flat bootstrap
/// model's durable record. Nothing in the linking critical path reads it:
/// the dialogue's reply hands the bootstrap tickets over directly. Restart
/// recovery does read it: the connection armer's sweep re-binds the data
/// namespace from this ticket when the node holds the directory alone.
pub(crate) const DATA_TICKET_KIND: &str = "data";

/// The undo of a create that does not reach its commit point: the data
/// namespace with its registration, the directory replica, and the session
/// classification once it is armed. Without it a failed create leaves the
/// running node reconciling replicas nobody hosts for the rest of the
/// process's life — the record on disk names none of them, so a restart
/// forgets them, but a restart is not what a caller retrying on a full
/// disk gets.
///
/// A guard rather than a helper because a create can also end without an
/// error: the caller's future is dropped — an HTTP client that
/// disconnected mid-request — and only `Drop` runs then. The undo touches
/// node-level state alone, never the runtime's coarse lock, so it can run
/// from the drop path without meeting the lock its own ceremony holds.
struct CreateRollback {
    node: Arc<data_layer::SyncNode>,
    identity: PdnId,
    directory_namespace: data_layer::NamespaceId,
    hosting_armed: bool,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
    armed: bool,
}

impl CreateRollback {
    fn new(
        node: Arc<data_layer::SyncNode>,
        identity: PdnId,
        directory_namespace: data_layer::NamespaceId,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            node,
            identity,
            directory_namespace,
            hosting_armed: false,
            cleanup_tasks,
            armed: true,
        }
    }

    /// Record that session classification is armed for this identity, so
    /// an undo from here on disarms it too.
    fn armed_hosting(&mut self) {
        self.hosting_armed = true;
    }

    /// Undo what the create provisioned, then disarm.
    async fn roll_back(&mut self) {
        undo_create(
            &self.node,
            self.identity,
            self.directory_namespace,
            self.hosting_armed,
        )
        .await;
        self.disarm();
    }

    /// The create committed: there is nothing to undo.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreateRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let node = Arc::clone(&self.node);
        let (identity, directory_namespace, hosting_armed) =
            (self.identity, self.directory_namespace, self.hosting_armed);
        // Through the supervisor, so shutdown waits for it rather than
        // racing it — the same tracker linking's rollback uses.
        let _detached = self.cleanup_tasks.spawn(async move {
            undo_create(&node, identity, directory_namespace, hosting_armed).await;
        });
    }
}

/// Best effort throughout: an undo that fails leaves more behind than one
/// that succeeds, and neither is worth failing the error the caller is
/// already receiving.
async fn undo_create(
    node: &data_layer::SyncNode,
    identity: PdnId,
    directory_namespace: data_layer::NamespaceId,
    hosting_armed: bool,
) {
    if hosting_armed {
        let _ = node.unhost_identity(identity);
    }
    // Unregisters the issuer as well as dropping the replica; an issuer
    // that never got that far refuses here, which is the same nothing.
    let _ = node.forget_namespace(identity).await;
    let _ = node.forget_doc(directory_namespace).await;
}

/// Creating and linking identities on a runtime. The production
/// implementation mints placeholder identifiers with no key material
/// behind them.
#[allow(async_fn_in_trait)]
pub trait IdentityService {
    /// Create an identity on this runtime — its first device: mint a fresh
    /// placeholder [`PdnId`] (a random identifier, no key material) and
    /// provision its store set — the private-metadata directory with this
    /// device registered, and the data namespace, whose ticket is published
    /// in the directory under the `data` kind.
    async fn create(&self) -> Result<PdnId>;

    /// Mint a linking invite for hosted `identity`: a one-time secret with
    /// a short lifetime (a default unless `lifetime` overrides it), pending
    /// on this runtime, and the self-contained payload the new device
    /// consumes. The payload carries no bearer material — no tickets and no
    /// identity proof; the bootstrap tickets ride the dialogue's reply.
    async fn linking_invite(
        &self,
        identity: PdnId,
        lifetime: Option<Duration>,
    ) -> Result<LinkingPayload>;

    /// Link this runtime as a device of the payload's identity, one
    /// explicit act per identity: dial the payload's address on the linking
    /// ALPN, present the secret, and import the directory and data
    /// namespace from the reply. Does not return success until the imported
    /// directory has completed one successful sync exchange. `timeout` is
    /// the budget of the whole act: the dialogue spends from it first — a
    /// dialed inviter that never answers fails as
    /// [`DialogueTimeout`](crate::linking::DialogueTimeout) — and the
    /// catch-up gets what remains
    /// ([`CatchUpTimeout`](crate::CatchUpTimeout)); either way the
    /// failed attempt leaves nothing behind on this runtime. A payload
    /// version this runtime does not speak ([`UnsupportedLinkingVersion`])
    /// and an identity it already hosts are refused before dialing.
    async fn link(&self, payload: LinkingPayload, timeout: Duration) -> Result<()>;
}

/// The production [`IdentityService`], backed by the runtime's `data-layer`
/// stack.
#[derive(Clone, Copy)]
pub struct RuntimeIdentityService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeIdentityService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }
}

impl IdentityService for RuntimeIdentityService<'_> {
    async fn create(&self) -> Result<PdnId> {
        let identity = PdnId::from_bytes(rand::random());
        // The node and the cleanup tracker are taken under the lock and
        // used without it: provisioning is several replica writes and a
        // ticket mint, none of which needs the runtime's coarse lock, and
        // holding it across all of them would put every other caller —
        // every service call, every identity's sweep — behind one
        // ceremony's disk and store work.
        let (node, cleanup_tasks) = {
            let state = self.runtime.state.lock().await;
            (Arc::clone(&state.node), state.cleanup_tasks.clone())
        };
        // The directory, with this device registered. Registration is
        // immediate — the store is fresh, there is no first sync for the
        // local write to race, and no other device holds a ticket to it,
        // so nothing written here can reach anyone.
        let directory = PrivateMetadataStore::create(&node).await?;
        // Armed as soon as there is a replica to undo. Every exit before
        // the commit point below goes through it — an error explicitly, a
        // dropped future through its `Drop` — so a create that does not
        // finish leaves neither replicas the node keeps reconciling nor an
        // identity armed for classification.
        let mut rollback = CreateRollback::new(
            Arc::clone(&node),
            identity,
            directory.namespace(),
            cleanup_tasks,
        );
        let provisioned = async {
            directory.add_device(node.node_id()).await?;
            // The data namespace, its ticket published as the directory's
            // durable record (the reply of a later linking hands over a
            // fresh one instead of reading this entry).
            node.create_namespace(identity).await?;
            let data_ticket = node
                .share_ticket(
                    identity,
                    ShareMode::Write,
                    AddrInfoOptions::RelayAndAddresses,
                )
                .await?;
            directory.put_ticket(DATA_TICKET_KIND, &data_ticket).await?;
            // The directory arms session classification for this identity
            // — its device records decide who is an own device, and its
            // data namespace serves fail-closed. The armer's subscription
            // is taken before the handle moves into the hosted set;
            // connections this identity establishes or learns of by
            // replication then register as their records arrive, not as a
            // side effect of the first grant read.
            let changes = directory.changes().await?;
            node.host_identity(identity, &directory)?;
            anyhow::Ok(changes)
        }
        .await;
        let changes = match provisioned {
            Ok(changes) => changes,
            Err(err) => {
                rollback.roll_back().await;
                return Err(err);
            }
        };
        rollback.armed_hosting();
        // The commit point, and the last step that can fail: the store set
        // is provisioned and armed, and what remains after the record is
        // written cannot fail at all. A process that dies before this line
        // leaves replicas nothing points at; a failed write (a full disk)
        // fails the create, leaves the previous record and every identity
        // it names intact, and takes this identity's replicas back with it.
        //
        // The lock is taken here and not before: the record is built from
        // the hosted set, so building it and adding to it are one act.
        let mut state = self.runtime.state.lock().await;
        if let Err(err) = state.commit_hosting(identity, directory.namespace()).await {
            drop(state);
            rollback.roll_back().await;
            return Err(err);
        }
        state
            .identities
            .insert(identity, HostedIdentity { directory });
        drop(state);
        crate::connections::spawn_connection_armer(
            Arc::downgrade(&self.runtime.state),
            identity,
            changes,
        );
        rollback.disarm();
        Ok(identity)
    }

    async fn linking_invite(
        &self,
        identity: PdnId,
        lifetime: Option<Duration>,
    ) -> Result<LinkingPayload> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        let secret = state.pending_linking_invites.mint(
            identity,
            lifetime.unwrap_or(DEFAULT_INVITE_LIFETIME),
            Instant::now(),
        )?;
        Ok(LinkingPayload {
            version: LINKING_FORMAT_VERSION,
            inviter_addr: state.node.dial_handle().addr(),
            secret,
            identity,
        })
    }

    async fn link(&self, payload: LinkingPayload, timeout: Duration) -> Result<()> {
        // The version refusal precedes the dial; the already-hosted refusal
        // runs inside the dialogue, also before dialing. The dialogue takes
        // the runtime lock per phase and never holds it across the network
        // round-trip or its catch-up wait — see `link_via_dialogue`.
        if payload.version != LINKING_FORMAT_VERSION {
            return Err(UnsupportedLinkingVersion {
                version: payload.version,
            }
            .into());
        }
        link_via_dialogue(&self.runtime.state, &payload, timeout).await
    }
}
