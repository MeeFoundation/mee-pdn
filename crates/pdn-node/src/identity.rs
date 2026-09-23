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

/// The directory kind of the identity's own data-namespace ticket. The
/// linking critical path never reads it (the reply hands the tickets over
/// directly); restart recovery re-binds the data namespace from it.
pub(crate) const DATA_TICKET_KIND: &str = "data";

/// The undo of a create that does not reach its commit point. A guard
/// rather than a helper because a create can also end by its future being
/// dropped — an HTTP client that disconnected — and only `Drop` runs then.
/// The undo touches node-level state alone, never the runtime's coarse
/// lock, so it can run from the drop path.
struct CreateRollback {
    node: Arc<data_layer::SyncNode>,
    identity: PdnId,
    /// `None` until the directory exists: the identity's half of the node
    /// is brought up before it, and that half is the first thing a failure
    /// has to take back down.
    directory_namespace: Option<data_layer::NamespaceId>,
    hosting_armed: bool,
    cleanup_tasks: crate::runtime::CleanupSupervisor,
    armed: bool,
}

impl CreateRollback {
    /// Armed as soon as the identity is provisioned, which is the first
    /// act with anything to undo: an actor thread, an open store and the
    /// entry in the hosted set, all of which a failure below would
    /// otherwise leave behind for the life of the process.
    fn new(
        node: Arc<data_layer::SyncNode>,
        identity: PdnId,
        cleanup_tasks: crate::runtime::CleanupSupervisor,
    ) -> Self {
        Self {
            node,
            identity,
            directory_namespace: None,
            hosting_armed: false,
            cleanup_tasks,
            armed: true,
        }
    }

    fn armed_directory(&mut self, namespace: data_layer::NamespaceId) {
        self.directory_namespace = Some(namespace);
    }

    fn armed_hosting(&mut self) {
        self.hosting_armed = true;
    }

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
        // Through the supervisor, so shutdown waits for it.
        let _detached = self.cleanup_tasks.spawn(async move {
            undo_create(&node, identity, directory_namespace, hosting_armed).await;
        });
    }
}

/// Best effort: the caller is already receiving an error. Dropping the
/// identity's half of the node takes its replicas with it, so the two
/// stores this create brought up need no separate undo — they are named
/// for the case where the identity stays.
async fn undo_create(
    node: &data_layer::SyncNode,
    identity: PdnId,
    directory_namespace: Option<data_layer::NamespaceId>,
    _hosting_armed: bool,
) {
    let _ = node.forget_namespace(identity, identity).await;
    if let Some(directory) = directory_namespace {
        let _ = node.forget_doc(identity, directory).await;
    }
    let _ = node.unhost_identity(identity).await;
}

/// Creating and linking identities on a runtime.
#[allow(async_fn_in_trait)]
pub trait IdentityService {
    /// Create an identity on its first device: a placeholder [`PdnId`] (a
    /// random identifier, no key material) with its store set provisioned.
    async fn create(&self) -> Result<PdnId>;

    /// Mint a linking invite for hosted `identity`; `lifetime` overrides
    /// the short default. The payload carries no bearer material.
    async fn linking_invite(
        &self,
        identity: PdnId,
        lifetime: Option<Duration>,
    ) -> Result<LinkingPayload>;

    /// Link this runtime as a device of the payload's identity, returning
    /// once the imported directory has completed one successful sync
    /// exchange. `timeout` is the budget of the whole act: the dialogue
    /// spends from it first ([`DialogueTimeout`](crate::linking::DialogueTimeout)),
    /// the catch-up gets what remains ([`CatchUpTimeout`](crate::CatchUpTimeout));
    /// a failed attempt leaves nothing behind. An unsupported payload
    /// version and an identity already hosted here are refused before
    /// dialing.
    async fn link(&self, payload: LinkingPayload, timeout: Duration) -> Result<()>;
}

/// The production [`IdentityService`].
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
        // Provisioning needs no coarse lock, and holding it across the
        // store work would put every other caller behind one ceremony.
        let (node, cleanup_tasks, injected_failure) = {
            #[cfg(feature = "test-util")]
            let mut state = self.runtime.state.lock().await;
            #[cfg(not(feature = "test-util"))]
            let state = self.runtime.state.lock().await;
            #[cfg(feature = "test-util")]
            let injected_failure = std::mem::take(&mut state.fail_next_directory_create);
            #[cfg(not(feature = "test-util"))]
            let injected_failure = false;
            (
                Arc::clone(&state.node),
                state.cleanup_tasks.clone(),
                injected_failure,
            )
        };
        // The identity's own half of the node first: its replicas live in
        // that store and nowhere else (ADR-0013).
        node.provision_identity(identity).await?;
        let mut rollback = CreateRollback::new(Arc::clone(&node), identity, cleanup_tasks);
        let created = if injected_failure {
            Err(anyhow::anyhow!("directory creation failed for test"))
        } else {
            PrivateMetadataStore::create(&node, identity).await
        };
        let directory = match created {
            Ok(directory) => directory,
            Err(err) => {
                rollback.roll_back().await;
                return Err(err);
            }
        };
        rollback.armed_directory(directory.namespace());
        let provisioned = async {
            // Before the commit point, unlike a link's confirmation: no other
            // device holds a ticket to this fresh directory, so nothing
            // written here can reach anyone.
            directory.add_device(node.node_id()).await?;
            node.create_namespace(identity, identity).await?;
            let data_ticket = node
                .share_ticket(
                    identity,
                    identity,
                    ShareMode::Write,
                    AddrInfoOptions::RelayAndAddresses,
                )
                .await?;
            directory.put_ticket(DATA_TICKET_KIND, &data_ticket).await?;
            // The armer's subscription, taken before the handle moves into
            // the hosted set.
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
        // The commit point, and the last step that can fail. The lock is
        // held across it, so the record and the hosted set change as one act.
        let mut state = self.runtime.state.lock().await;
        if let Err(err) = state.commit_hosting(identity, directory.namespace()).await {
            drop(state);
            rollback.roll_back().await;
            return Err(err);
        }
        let author = node.default_author(identity)?;
        state
            .identities
            .insert(identity, HostedIdentity { directory, author });
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
        if payload.version != LINKING_FORMAT_VERSION {
            return Err(UnsupportedLinkingVersion {
                version: payload.version,
            }
            .into());
        }
        link_via_dialogue(&self.runtime.state, &payload, timeout).await
    }
}
