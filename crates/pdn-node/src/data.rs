//! The data service: entries in data namespaces hosted on this node, plus
//! the namespace ticket handover.

use anyhow::Result;
use data_layer::{AddrInfoOptions, DocTicket, GrantRead, ShareMode};
use pdn_types::{EntryInfo, EntryPath, PdnId};

use crate::runtime::{Runtime, State};

/// A write addressed a granted namespace outside the write set of the
/// locally replicated grant record — refused at the call site, before the
/// replica is touched. A courtesy to honest writers: the enforcement proper
/// is the issuer-side ingest gate, and a bypass ends in retraction.
#[derive(Debug, Clone, thiserror::Error)]
#[error("write to {issuer} at {path} is not covered by the local grant's write set")]
pub struct WriteNotGranted {
    /// The granted namespace's issuer.
    pub issuer: PdnId,
    /// The path the write addressed.
    pub path: EntryPath,
}

/// Writing, reading, and listing entries by issuer and path, and the
/// namespace-ticket handover: share a namespace hosted here, import a
/// peer's.
///
/// The connections service's grant surface is the sanctioned transport for
/// namespace access between connected identities.
/// The out-of-band share/import here does not deliver from an armed
/// issuer: creation arms fail-closed serving, so a bare ticket without a
/// recorded grant is refused; the surface remains for un-armed assemblies
/// and as the paired-denial material.
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
    /// import: the namespace handover.
    async fn share(&self, issuer: PdnId, mode: ShareMode) -> Result<DocTicket>;

    /// Import a peer's data namespace from a `ticket` obtained **out of
    /// band**, registering it under `issuer` (named by the caller — the
    /// ticket carries the namespace, not its issuer). For a namespace
    /// reached through a connection's grant this is not needed: the runtime
    /// binds grants by itself, and a namespace imported here is never
    /// unbound by the grant sweep.
    ///
    /// The replica registers under `issuer`, stays outside the gossip swarm
    /// — that swarm is the issuer's device set — and is re-served only to
    /// the devices of the grant's audience identity, per the locally
    /// replicated grant record. What the session actually delivers is the
    /// issuer's grant record to decide, not the import: with no record
    /// behind it this ticket delivers nothing.
    async fn import(&self, issuer: PdnId, ticket: DocTicket) -> Result<()>;

    /// [`import`](Self::import) for a ticket that arrived with a grant —
    /// same registration, same stance. Reads nudge a reconciliation and are
    /// served from the local replica.
    async fn import_scoped(&self, issuer: PdnId, ticket: DocTicket) -> Result<()>;
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

    /// Write past the courtesy refusal, letting the issuer's ingest gate be
    /// the only boundary. Behind the `test-util` feature and absent from
    /// every product build: it exists to let a scenario test deterministically
    /// produce an entry the issuer refuses — the same entry that arises in
    /// the field from a stale local grant (courtesy allows, the fresh gate
    /// refuses) or an adversarial client that never calls the guarded
    /// [`write`](DataService::write) at all. For a granted namespace the
    /// write leaves this node's replica at once (the grant's write ticket
    /// carries the namespace secret); the issuer admits it only if its own
    /// grant record covers the claim, and refuses it otherwise, after which
    /// the provisional entry is retracted here (Invariant 2, the write side).
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
}

impl DataService for RuntimeDataService<'_> {
    async fn write(&self, issuer: PdnId, path: &EntryPath, payload: &[u8]) -> Result<()> {
        let state = self.runtime.state.lock().await;
        // Courtesy refusal on a grant-bound namespace: judged from the
        // locally replicated grant records, so the error arrives at the
        // call site instead of a bounded number of sessions later. Not the
        // enforcement — the issuer's gate is.
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
        let _displaced = state.node.import_namespace_granted(issuer, ticket).await?;
        Ok(())
    }

    async fn import_scoped(&self, issuer: PdnId, ticket: DocTicket) -> Result<()> {
        let state = self.runtime.state.lock().await;
        // Same rebinding contract as `import` — and the same grantee stance
        // below it (contacts-only sync, audience-device re-serving); the
        // issuer's grant record is what scopes the view.
        let _displaced = state.node.import_namespace_scoped(issuer, ticket).await?;
        Ok(())
    }
}

/// The courtesy verdict for a write at `path` under `issuer`: `None` allows.
/// A namespace this runtime's grant binder bound is judged by the grants
/// behind it — one grant whose write set covers the claim is enough to allow,
/// and the refusal stands only when every grant on that issuer was read and
/// none of them covers it. A namespace not bound by a grant — a hosted
/// identity's own, or an out-of-band import — is not judged here at all: the
/// issuer's gate is the boundary.
///
/// A grant this node cannot read *right now* allows: a record whose payload
/// is still replicating (the window a republish opens) says nothing about
/// what it covers, and a courtesy that guesses there refuses honest writes
/// the issuer would keep. A record absent altogether does mean not covered —
/// that is a withdrawal, and refusing spares the writer a retraction.
async fn write_refusal(
    state: &State,
    issuer: PdnId,
    path: &EntryPath,
) -> Result<Option<WriteNotGranted>> {
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
            // Read whole and not covering the claim, or decidedly no grant
            // here (withdrawn, or granting someone else); another pair may
            // still cover it.
            Ok(GrantRead::Granted(..) | GrantRead::None) => {}
            // What this pair grants is not knowable right now, and a courtesy
            // does not refuse on a guess.
            Ok(GrantRead::Unreadable) | Err(_) => return Ok(None),
        }
    }
    Ok(grant_bound.then(|| WriteNotGranted {
        issuer,
        path: path.clone(),
    }))
}
