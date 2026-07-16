//! The identity service: create an identity on its first device, link every
//! further device from the identity's seed.

use std::time::Duration;

use anyhow::{bail, Result};
use data_layer::{link_device, provision_identity, AddrInfoOptions, DocTicket, ShareMode};
use pdn_types::PdnId;

use crate::runtime::Runtime;

/// Creating and linking identities on a runtime.
///
/// A trait because a second implementation is a live prospect: the
/// production service mints placeholder identifiers with no key material
/// behind them, and a KERI-backed service is the future replacement.
#[allow(async_fn_in_trait)]
pub trait IdentityService {
    /// Create an identity on this runtime — its first device: mint a fresh
    /// placeholder [`PdnId`] (a random identifier, no key material yet),
    /// provision the identity's store set, and create its data namespace.
    async fn create(&self) -> Result<PdnId>;

    /// The seed for linking another device into hosted `identity` — its
    /// private-metadata-store ticket, handed to the linking device out of
    /// band.
    async fn linking_seed(&self, identity: PdnId) -> Result<DocTicket>;

    /// Link this runtime as a device of `identity` from its `seed`, one
    /// explicit act per identity. The caller names the identity for the
    /// same reason it holds the seed: both arrive out of band — the seed
    /// carries store access, not the identity's id. Waits up to `timeout`
    /// for the identity's directory to sync in; a stalled directory
    /// surfaces as an error, not a hang.
    async fn link(&self, identity: PdnId, seed: DocTicket, timeout: Duration) -> Result<()>;
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
        let stores = provision_identity(&state.node).await?;
        state.node.create_namespace(identity).await?;
        state.identities.insert(identity, stores);
        Ok(identity)
    }

    async fn linking_seed(&self, identity: PdnId) -> Result<DocTicket> {
        let state = self.runtime.state.lock().await;
        // Write access and dialable addresses, as at provisioning: every
        // device of the identity writes to the directory.
        let seed = state
            .hosted(identity)?
            .private_metadata
            .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await?;
        Ok(seed)
    }

    async fn link(&self, identity: PdnId, seed: DocTicket, timeout: Duration) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        if state.identities.contains_key(&identity) {
            bail!("identity already hosted on this runtime: {identity}");
        }
        let stores = link_device(&state.node, seed, timeout).await?;
        state.identities.insert(identity, stores);
        Ok(())
    }
}
