//! The data service: entries in data namespaces hosted on this node, plus
//! the interim whole-store ticket handover.

use anyhow::Result;
use data_layer::{AddrInfoOptions, DocTicket, ShareMode};
use pdn_types::{EntryInfo, EntryPath, PdnId};

use crate::runtime::Runtime;

/// Writing, reading, and listing entries by issuer and path, and the
/// interim namespace-ticket handover: share a namespace hosted here, import
/// a peer's.
///
/// Whole-store tickets are the interim access model (the ADR-0008 posture:
/// possession of a replica's ticket bounds access to it). The connections
/// service's grant surface is the honest transport for these tickets
/// between connected identities; the out-of-band share/import here remains
/// for namespaces outside any connection. Capability-scoped sharing
/// (subset-rbsr egress) replaces the grant payload, not the channel. A test
/// mock standing in for the network is the second implementation this
/// trait anticipates.
///
/// Operations address issuers whose data namespace was created or imported
/// on this node — not hosted identities: creation and linking both bring a
/// hosted identity's own namespace up (the linking reply carries its
/// ticket), while an imported peer namespace belongs to no hosted identity
/// at all.
#[allow(async_fn_in_trait)]
pub trait DataService {
    /// Write `payload` at `path` in the data namespace of `issuer`.
    async fn write(&self, issuer: PdnId, path: &EntryPath, payload: &[u8]) -> Result<()>;

    /// Read the latest payload at `path` under `issuer`. Returns `Ok(None)`
    /// both when no entry exists and when its record is stored but the
    /// payload has not synced yet (record-first reads); poll to observe
    /// convergence.
    async fn read(&self, issuer: PdnId, path: &EntryPath) -> Result<Option<Vec<u8>>>;

    /// List entry metadata under `issuer` — no payload bytes — optionally
    /// narrowed to paths under `path_prefix`, matching whole components.
    async fn list(&self, issuer: PdnId, path_prefix: Option<&EntryPath>) -> Result<Vec<EntryInfo>>;

    /// Share the data namespace of `issuer` as a ticket a peer runtime can
    /// import: the interim whole-store handover.
    async fn share(&self, issuer: PdnId, mode: ShareMode) -> Result<DocTicket>;

    /// Import a peer's data namespace from `ticket`, registering it under
    /// `issuer` (named by the caller — the ticket carries the namespace,
    /// not its issuer), after which its entries sync whole.
    async fn import(&self, issuer: PdnId, ticket: DocTicket) -> Result<()>;
}

/// The production [`DataService`], backed by the runtime's `data-layer`
/// stack.
#[derive(Clone, Copy)]
pub struct RuntimeDataService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeDataService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }
}

impl DataService for RuntimeDataService<'_> {
    async fn write(&self, issuer: PdnId, path: &EntryPath, payload: &[u8]) -> Result<()> {
        let state = self.runtime.state.lock().await;
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

    /// The foreign ticket is registered into the node and persisted nowhere
    /// else — in particular never into a hosted identity's device-replicated
    /// stores, where a copy would spread to every device and outlive the
    /// grant it came from. The runtime's registry is a cache, not the
    /// ticket's durable home.
    async fn import(&self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        // Importing an issuer this runtime already knows rebinds it, and the
        // displaced binding is dropped knowingly: with one namespace per
        // issuer a re-import resolves to the same replica, so what the
        // caller replaces is an equivalent handle. There is nothing to undo —
        // an explicit import is its own last word, unlike the linking
        // dialogue's, which must survive a failed catch-up.
        let _displaced = state.node.import_namespace(issuer, ticket).await?;
        Ok(())
    }
}
