//! The identity service: create an identity on its first device, link every
//! further device over the linking dialogue.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{AddrInfoOptions, PrivateMetadataStore, ShareMode};
use pdn_types::PdnId;

use crate::linking::{
    link_via_dialogue, LinkingPayload, UnsupportedLinkingVersion, LINKING_FORMAT_VERSION,
};
use crate::pairing::DEFAULT_INVITE_LIFETIME;
use crate::runtime::{HostedIdentity, Runtime};

/// The private-metadata directory kind under which an identity's own
/// data-namespace ticket is published at creation — the flat bootstrap
/// model's durable record. Nothing in the linking critical path reads it:
/// the dialogue's reply hands the bootstrap tickets over directly.
const DATA_TICKET_KIND: &str = "data";

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
    /// directory has completed one successful sync exchange — bounded by
    /// `timeout`, after which the attempt fails and leaves nothing behind
    /// on this runtime. A payload version this runtime does not speak
    /// ([`UnsupportedLinkingVersion`]) and an identity it already hosts are
    /// refused before dialing.
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
        let mut state = self.runtime.state.lock().await;
        // The directory, with this device registered. Registration is
        // immediate — the store is fresh, there is no first sync for the
        // local write to race.
        let directory = PrivateMetadataStore::create(&state.node).await?;
        directory.add_device(state.node.node_id()).await?;
        // The data namespace, its ticket published as the directory's
        // durable record (the reply of a later linking hands over a fresh
        // one instead of reading this entry).
        state.node.create_namespace(identity).await?;
        let data_ticket = state
            .node
            .share_ticket(
                identity,
                ShareMode::Write,
                AddrInfoOptions::RelayAndAddresses,
            )
            .await?;
        directory.put_ticket(DATA_TICKET_KIND, &data_ticket).await?;
        // The directory arms session classification for this identity —
        // its device records decide who is an own device, and its data
        // namespace serves fail-closed. The armer's subscription is taken
        // before the handle moves into the hosted set; connections this
        // identity establishes or learns of by replication then register
        // as their records arrive, not as a side effect of the first grant
        // read.
        let changes = directory.changes().await?;
        state.node.host_identity(identity, &directory)?;
        state
            .identities
            .insert(identity, HostedIdentity { directory });
        crate::connections::spawn_connection_armer(
            Arc::downgrade(&self.runtime.state),
            identity,
            changes,
        );
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
