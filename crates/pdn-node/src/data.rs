//! The data service: entries in data namespaces hosted on this node, plus
//! the out-of-band ticket handover.

use anyhow::Result;
use data_layer::{AddrInfoOptions, DocTicket, GrantRead, ShareMode};
#[cfg(feature = "test-util")]
use pdn_types::NodeId;
use pdn_types::{EntryInfo, EntryPath, PdnId};

use crate::runtime::{Runtime, State};

/// A write outside the write set of the locally replicated grant record,
/// refused at the call site. A courtesy: the enforcement proper is the
/// issuer-side ingest gate, and a bypass ends in retraction.
#[derive(Debug, Clone, thiserror::Error)]
#[error("write to {issuer} at {path} is not covered by the local grant's write set")]
pub struct WriteNotGranted {
    pub issuer: PdnId,
    pub path: EntryPath,
}

/// Entries by issuer and path, plus the ticket handover for a ticket
/// obtained out of band. Operations address issuers whose data namespace
/// was created or imported here — not hosted identities: an imported peer
/// namespace belongs to no hosted identity at all.
#[allow(async_fn_in_trait)]
pub trait DataService {
    async fn write(&self, issuer: PdnId, path: &EntryPath, payload: &[u8]) -> Result<()>;

    /// `Ok(None)` both when no entry exists and when its payload has not
    /// synced yet; poll to observe convergence.
    async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>>;

    /// Entry metadata, optionally narrowed to `path_prefix` matching whole
    /// components.
    async fn list(&self, issuer: PdnId, path_prefix: Option<&EntryPath>) -> Result<Vec<EntryInfo>>;

    async fn share(&self, issuer: PdnId, mode: ShareMode) -> Result<DocTicket>;

    /// Register a ticket obtained out of band under `issuer` (the ticket
    /// carries the namespace, not its issuer). Not needed for a namespace
    /// reached through a grant — the runtime binds those by itself, and
    /// never unbinds what was imported here. With no grant record behind it
    /// the ticket delivers nothing from an armed issuer.
    async fn import(&self, issuer: PdnId, ticket: DocTicket) -> Result<()>;

    /// [`import`](Self::import) for a ticket that arrived with a grant —
    /// same registration, same stance.
    async fn import_scoped(&self, issuer: PdnId, ticket: DocTicket) -> Result<()>;
}

/// The production [`DataService`].
#[derive(Clone, Copy)]
pub struct RuntimeDataService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeDataService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }

    /// Write past the courtesy refusal, so a scenario produces an entry the
    /// issuer's gate refuses — the entry a stale local grant or an
    /// adversarial client produces in the field.
    #[cfg(feature = "test-util")]
    pub async fn write_unguarded(
        &self,
        issuer: PdnId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let state = self.runtime.state.lock().await;
        state.node.write(issuer, state.author, path, payload).await
    }

    /// The devices among the contacts the grant sweep derived for
    /// `issuer`'s namespace. Device ids rather than addresses, so a host
    /// depending on `pdn-node` alone can name the type.
    #[cfg(feature = "test-util")]
    pub async fn contacts_of(&self, issuer: PdnId) -> Result<Vec<NodeId>> {
        let state = self.runtime.state.lock().await;
        Ok(state
            .node
            .namespace_contacts(issuer)?
            .iter()
            .map(|contact| NodeId::from_bytes(*contact.id.as_bytes()))
            .collect())
    }

    /// Forget `issuer`'s replica out from under the grant binder's memo — a
    /// memo/registry desync the product paths never produce.
    #[cfg(feature = "test-util")]
    pub async fn forget_namespace(&self, issuer: PdnId) -> Result<()> {
        let state = self.runtime.state.lock().await;
        state.node.forget_namespace(issuer).await
    }
}

impl DataService for RuntimeDataService<'_> {
    async fn write(&self, issuer: PdnId, path: &EntryPath, payload: &[u8]) -> Result<()> {
        let state = self.runtime.state.lock().await;
        if let Some(refused) = write_refusal(&state, issuer, path).await? {
            return Err(refused.into());
        }
        state.node.write(issuer, state.author, path, payload).await
    }

    async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>> {
        let state = self.runtime.state.lock().await;
        state.node.read(issuer, path).await
    }

    async fn list(&self, issuer: PdnId, path_prefix: Option<&EntryPath>) -> Result<Vec<EntryInfo>> {
        let state = self.runtime.state.lock().await;
        state.node.list(issuer, path_prefix).await
    }

    async fn share(&self, issuer: PdnId, mode: ShareMode) -> Result<DocTicket> {
        let state = self.runtime.state.lock().await;
        state
            .node
            .share_ticket(issuer, mode, AddrInfoOptions::RelayAndAddresses)
            .await
    }

    /// The ticket is registered into the node and persisted nowhere else —
    /// never into a device-replicated store, where a copy would outlive the
    /// grant it came from.
    async fn import(&self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        // The displaced binding is dropped knowingly: with one namespace per
        // issuer a re-import resolves to the same replica. Nothing to undo —
        // an explicit import is its own last word.
        let _displaced = state.node.import_namespace_granted(issuer, ticket).await?;
        Ok(())
    }

    async fn import_scoped(&self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        let _displaced = state.node.import_namespace_scoped(issuer, ticket).await?;
        Ok(())
    }
}

/// The courtesy verdict: `None` allows. Only a namespace the grant binder
/// bound is judged; one grant covering the claim allows, and the refusal
/// stands only when every grant on that issuer was read and none covers
/// it. A grant this node cannot read right now allows — a payload still
/// replicating says nothing about what it covers — while a record absent
/// altogether is a withdrawal, and refusing spares the writer a retraction.
async fn write_refusal(
    state: &State,
    issuer: PdnId,
    path: &EntryPath,
) -> Result<Option<WriteNotGranted>> {
    // Never judged by grant courtesy, even when this runtime also holds a
    // grant record keyed by the same issuer (a hosted peer granting to
    // another identity hosted here): that record is not a bound-in copy of
    // the issuer's own namespace.
    if state.is_hosted(issuer) {
        return Ok(None);
    }
    let mut grant_bound = false;
    for (bound_identity, bound_peer, bound_issuer) in state.bound_grants.keys() {
        if *bound_issuer != issuer {
            continue;
        }
        grant_bound = true;
        let Some(pair) = state.metadata_pairs.get(&(*bound_identity, *bound_peer)) else {
            continue;
        };
        match pair.peer.read_grant(issuer, *bound_identity).await {
            Ok(GrantRead::Granted(grant, _ticket)) if grant.covers_write(path) => return Ok(None),
            // Another pair may still cover it.
            Ok(GrantRead::Granted(..) | GrantRead::None) => {}
            // A courtesy does not refuse on a guess.
            Ok(GrantRead::Unreadable) | Err(_) => return Ok(None),
        }
    }
    Ok(grant_bound.then(|| WriteNotGranted {
        issuer,
        path: path.clone(),
    }))
}
