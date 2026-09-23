//! The data service: entries in data namespaces hosted on this node, plus
//! the out-of-band ticket handover.

use anyhow::Result;
#[cfg(feature = "test-util")]
use data_layer::NamespaceId;
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
/// obtained out of band. Every operation names the identity performing it
/// and acts for that identity alone (ADR-0013): an issuer a co-located
/// identity holds is unknown to the caller, exactly as an issuer no
/// identity here holds is. An operation naming an identity this node does
/// not host is refused as [`UnknownIdentity`](crate::UnknownIdentity),
/// which is the answer a refused link leaves behind.
#[allow(async_fn_in_trait)]
pub trait DataService {
    async fn write(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()>;

    /// `Ok(None)` both when no entry exists and when its payload has not
    /// synced yet; poll to observe convergence.
    async fn read(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>>;

    /// Entry metadata, optionally narrowed to `path_prefix` matching whole
    /// components.
    async fn list(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>>;

    /// A ticket on a namespace `identity` issues; one it holds under a grant
    /// or imported out of band is refused as
    /// [`GranteeCannotShare`](crate::GranteeCannotShare).
    async fn share(&self, identity: PdnId, issuer: PdnId, mode: ShareMode) -> Result<DocTicket>;

    /// Register a ticket obtained out of band under `issuer`, held for
    /// `identity` (the ticket carries the namespace, not its issuer). Not
    /// needed for a namespace reached through a grant — the runtime binds
    /// those by itself, and never unbinds what was imported here. With no
    /// grant record behind it the ticket delivers nothing from an armed
    /// issuer, and this node re-serves it to no one. Refused under
    /// `identity`'s own id; an issuer already resolving to the ticket's
    /// replica is left as it is.
    async fn import(&self, identity: PdnId, issuer: PdnId, ticket: DocTicket) -> Result<()>;

    /// [`import`](Self::import) for a ticket that arrived with a grant —
    /// same registration, same stance.
    async fn import_scoped(&self, identity: PdnId, issuer: PdnId, ticket: DocTicket) -> Result<()>;
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
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let state = self.runtime.state.lock().await;
        let author = state.hosted(identity)?.author;
        state
            .node
            .write(identity, issuer, author, path, payload)
            .await
    }

    /// The devices among the contacts the grant sweep derived for
    /// `issuer`'s namespace. Device ids rather than addresses, so a host
    /// depending on `pdn-node` alone can name the type.
    #[cfg(feature = "test-util")]
    pub async fn contacts_of(&self, identity: PdnId, issuer: PdnId) -> Result<Vec<NodeId>> {
        let state = self.runtime.state.lock().await;
        Ok(state
            .node
            .namespace_contacts(identity, issuer)?
            .iter()
            .map(|contact| NodeId::from_bytes(*contact.addr.id.as_bytes()))
            .collect())
    }

    /// Whether `identity` still holds `namespace`, stored or reconciled.
    #[cfg(feature = "test-util")]
    pub async fn holds_replica(&self, identity: PdnId, namespace: NamespaceId) -> Result<bool> {
        let state = self.runtime.state.lock().await;
        state.node.holds_replica(identity, namespace).await
    }

    /// Forget `issuer`'s replica out from under the grant binder's memo — a
    /// memo/registry desync the product paths never produce.
    #[cfg(feature = "test-util")]
    pub async fn forget_namespace(&self, identity: PdnId, issuer: PdnId) -> Result<()> {
        let state = self.runtime.state.lock().await;
        state.node.forget_namespace(identity, issuer).await
    }
}

impl DataService for RuntimeDataService<'_> {
    async fn write(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<()> {
        let state = self.runtime.state.lock().await;
        let author = state.hosted(identity)?.author;
        if let Some(refused) = write_refusal(&state, identity, issuer, path).await? {
            return Err(refused.into());
        }
        state
            .node
            .write(identity, issuer, author, path, payload)
            .await
    }

    async fn read(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>> {
        let state = self.runtime.state.lock().await;
        let _hosted = state.hosted(identity)?;
        state.node.read(identity, issuer, path).await
    }

    async fn list(
        &self,
        identity: PdnId,
        issuer: PdnId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Vec<EntryInfo>> {
        let state = self.runtime.state.lock().await;
        let _hosted = state.hosted(identity)?;
        state.node.list(identity, issuer, path_prefix).await
    }

    async fn share(&self, identity: PdnId, issuer: PdnId, mode: ShareMode) -> Result<DocTicket> {
        let state = self.runtime.state.lock().await;
        let _hosted = state.hosted(identity)?;
        state
            .node
            .share_ticket(identity, issuer, mode, AddrInfoOptions::RelayAndAddresses)
            .await
    }

    /// The ticket is registered into the node and persisted nowhere else —
    /// never into a device-replicated store, where a copy would outlive the
    /// grant it came from.
    async fn import(&self, identity: PdnId, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        let _hosted = state.hosted(identity)?;
        let _import = state
            .node
            .import_namespace_granted(identity, issuer, ticket)
            .await?;
        Ok(())
    }

    async fn import_scoped(&self, identity: PdnId, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        let _hosted = state.hosted(identity)?;
        let _import = state
            .node
            .import_namespace_scoped(identity, issuer, ticket)
            .await?;
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
    identity: PdnId,
    issuer: PdnId,
    path: &EntryPath,
) -> Result<Option<WriteNotGranted>> {
    // An identity's own namespace is never judged by grant courtesy: no
    // grant on it can be expressed. A namespace held under a grant is
    // judged wherever its issuer lives, co-located included (ADR-0013) —
    // otherwise a write outside the write set is answered with success
    // here and undone by the issuer's retraction later.
    if issuer == identity {
        return Ok(None);
    }
    let mut grant_bound = false;
    for (bound_identity, bound_peer, bound_issuer) in state.bound_grants.keys() {
        if *bound_issuer != issuer || *bound_identity != identity {
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
