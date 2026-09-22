//! The connections service: establish a hosted identity's connections,
//! list them, and carry grants over the connections' metadata pairs.

use std::{
    collections::HashSet,
    sync::Weak,
    time::{Duration, Instant},
};

use anyhow::Result;
use data_layer::{
    holder_of, AddrInfoOptions, ConnectionMetadata, ConnectionMetadataStore, Contact, DocTicket,
    EndpointAddr, EndpointId, GrantedClaim, ReadGrant, ShareMode,
};
use futures_lite::{Stream, StreamExt};
use pdn_types::{NodeId, NonEmpty, PdnId};
use tokio::sync::Mutex;

use crate::{
    pairing::{
        establish_via_dialogue, InvitePayload, UnsupportedInviteVersion, DEFAULT_INVITE_LIFETIME,
        INVITE_FORMAT_VERSION,
    },
    retraction::apply_retractions,
    runtime::{Runtime, State},
};

/// Granting another identity's data is delegation, which is not
/// expressible: the classifier reads the connections of the *data issuer's*
/// identity, so such a grant would publish, replicate, and enforce as
/// nothing — a silent no-op on both sides.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "identity {identity} cannot grant data issued by {issuer}: granting another identity's data \
     is delegation, which is not expressible"
)]
pub struct DelegationUnsupported {
    pub identity: PdnId,
    pub issuer: PdnId,
}

/// No connection metadata pair toward `peer`. Downcast from the
/// `anyhow::Error` of `publish_grant` / `withdraw_grant`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no connection metadata pair toward {peer}")]
pub struct PeerNotConnected {
    pub identity: PdnId,
    pub peer: PdnId,
}

/// A grant read from a connected peer's metadata store. An observation:
/// the grant binder is what imports the granted namespace.
#[derive(Debug, Clone)]
pub struct PeerGrant {
    pub grant: ReadGrant,
    /// Read-mode for read-only grants (no namespace secret), write-mode
    /// with write.
    pub ticket: DocTicket,
}

/// Establishing, listing, and granting over a hosted identity's
/// connections. Establishment is the producer of connections; no manual
/// recording is offered. Both grant reads report what this device holds at
/// the moment of the call and never wait: an empty answer covers no
/// connection, a pair not yet replicated here, a record whose payload is
/// not readable yet, and nothing granted alike. Neither read is free of
/// writes — opening a pair publishes this device's record and registers
/// the connection, the same acts the connection armer performs.
#[allow(async_fn_in_trait)]
pub trait ConnectionsService {
    /// Mint an invite for hosted `identity`; `lifetime` overrides the short
    /// default. The payload carries no bearer material.
    async fn invite(&self, identity: PdnId, lifetime: Option<Duration>) -> Result<InvitePayload>;

    /// An unsupported payload version is refused before dialing.
    async fn establish(&self, identity: PdnId, invite: InvitePayload) -> Result<()>;

    async fn list(&self, identity: PdnId) -> Result<Vec<PdnId>>;

    /// One tombstone over the single record; `issuer` must be `identity`
    /// itself. The grantee unbinds once the tombstone replicates, and the
    /// replica shared by co-hosted audiences (ADR-0009) leaves with the
    /// last grant that binds it.
    async fn withdraw_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()>;

    /// `identity` grants `peer` read — and, per claim, write — on exactly
    /// `claims` of `issuer`'s data; `issuer` must be `identity` itself
    /// ([`DelegationUnsupported`]). Capability and ticket travel as one
    /// record, replacing any previous grant for this issuer; the ticket's
    /// mode follows the grant's commands as a whole.
    async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<GrantedClaim>,
    ) -> Result<()>;

    /// What `peer` has published toward `identity`, capability and ticket.
    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>>;

    /// What `identity` has published toward `peer` — the capability alone,
    /// since the caller issues the namespace the record addresses. One at
    /// most, addressed rather than searched for: this half is written by
    /// `identity`'s own devices, and publishing refuses every other issuer.
    async fn read_own_grants(&self, identity: PdnId, peer: PdnId) -> Result<Option<ReadGrant>>;
}

/// The production [`ConnectionsService`].
#[derive(Clone, Copy)]
pub struct RuntimeConnectionsService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeConnectionsService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }

    /// The devices among the contacts of the pair's two halves — own first,
    /// peer second. Asserted directly because the engine's own recorded
    /// peers rescue the recovery it exists for often enough to hide the
    /// derivation behind luck. Empty lists when no pair is open.
    #[cfg(feature = "test-util")]
    pub async fn pair_contacts(
        &self,
        identity: PdnId,
        peer: PdnId,
    ) -> Result<(Vec<NodeId>, Vec<NodeId>)> {
        let state = self.runtime.state.lock().await;
        let Some(pair) = state.metadata_pairs.get(&(identity, peer)) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let devices = |contacts: Vec<Contact>| {
            contacts
                .iter()
                .map(|contact| NodeId::from_bytes(*contact.addr.id.as_bytes()))
                .collect::<Vec<_>>()
        };
        Ok((
            devices(state.node.doc_contacts(identity, pair.own.namespace())?),
            devices(state.node.doc_contacts(identity, pair.peer.namespace())?),
        ))
    }

    /// Whether the binder of `(identity, peer)` memoized an import for
    /// `issuer` — orders an assertion after the sweep that acted.
    #[cfg(feature = "test-util")]
    pub async fn grant_bound(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> bool {
        let state = self.runtime.state.lock().await;
        state.bound_grants.contains_key(&(identity, peer, issuer))
    }

    /// Drop the binder's memo of an import, replica untouched — the
    /// in-memory bookkeeping a restart clears.
    #[cfg(feature = "test-util")]
    pub async fn clear_grant_memo(&self, identity: PdnId, peer: PdnId, issuer: PdnId) {
        let mut state = self.runtime.state.lock().await;
        state.bound_grants.remove(&(identity, peer, issuer));
    }

    /// The counterparty's published devices, read as the grant sweep reads
    /// them; an unopened pair reads as publishing nothing.
    #[cfg(feature = "test-util")]
    pub async fn published_devices_of(&self, identity: PdnId, peer: PdnId) -> Result<Vec<NodeId>> {
        let state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        match state.metadata_pairs.get(&(identity, peer)) {
            Some(open) => open.peer.published_devices().await,
            None => Ok(Vec::new()),
        }
    }

    /// One grant sweep of `(identity, peer)`, synchronously; an unopened
    /// pair sweeps as nothing.
    #[cfg(feature = "test-util")]
    pub async fn sweep_pair_now(&self, identity: PdnId, peer: PdnId) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        let Some(open) = state.metadata_pairs.get(&(identity, peer)) else {
            return Ok(());
        };
        let peer_store = open.peer.clone();
        let _still_current = bind_grants(&mut state, identity, peer, &peer_store).await;
        Ok(())
    }
}

impl ConnectionsService for RuntimeConnectionsService<'_> {
    async fn invite(&self, identity: PdnId, lifetime: Option<Duration>) -> Result<InvitePayload> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        let secret = state.pending_invites.mint(
            identity,
            lifetime.unwrap_or(DEFAULT_INVITE_LIFETIME),
            Instant::now(),
        )?;
        Ok(InvitePayload {
            version: INVITE_FORMAT_VERSION,
            inviter_addr: state.node.dial_handle().addr(),
            secret,
            inviter: identity,
        })
    }

    async fn establish(&self, identity: PdnId, invite: InvitePayload) -> Result<()> {
        if invite.version != INVITE_FORMAT_VERSION {
            return Err(UnsupportedInviteVersion {
                version: invite.version,
            }
            .into());
        }
        establish_via_dialogue(&self.runtime.state, identity, &invite).await
    }

    async fn list(&self, identity: PdnId) -> Result<Vec<PdnId>> {
        let state = self.runtime.state.lock().await;
        state.hosted(identity)?.directory.list_connections().await
    }

    async fn withdraw_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        if identity != issuer {
            return Err(DelegationUnsupported { identity, issuer }.into());
        }
        let pair = open_pair(&mut state, identity, peer)
            .await?
            .ok_or(PeerNotConnected { identity, peer })?;
        pair.own.withdraw_grant(issuer).await
    }

    async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<GrantedClaim>,
    ) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        if identity != issuer {
            return Err(DelegationUnsupported { identity, issuer }.into());
        }
        let pair = open_pair(&mut state, identity, peer)
            .await?
            .ok_or(PeerNotConnected { identity, peer })?;
        let grant = ReadGrant {
            issuer,
            audience: peer,
            claims,
        };
        let mode = if grant.grants_any_write() {
            ShareMode::Write
        } else {
            ShareMode::Read
        };
        let ticket = state
            .node
            .share_ticket(identity, issuer, mode, AddrInfoOptions::RelayAndAddresses)
            .await?;
        pair.own.publish_grant(&grant, &ticket).await
    }

    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>> {
        let pair = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            open_pair(&mut state, identity, peer).await?
        };
        let Some(pair) = pair else {
            return Ok(Vec::new());
        };
        // The one key this counterparty can have written, read exactly
        // rather than found by scanning: a record keyed by a third identity
        // is its word over data that is not its own, and the binder ignores
        // it, so reporting it as a grant would describe a state no read or
        // write follows.
        Ok(pair
            .peer
            .read_grant(peer, identity)
            .await?
            .granted()
            .map(|(grant, ticket)| PeerGrant { grant, ticket })
            .into_iter()
            .collect())
    }

    async fn read_own_grants(&self, identity: PdnId, peer: PdnId) -> Result<Option<ReadGrant>> {
        let pair = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            open_pair(&mut state, identity, peer).await?
        };
        let Some(pair) = pair else {
            return Ok(None);
        };
        // The one key this identity can have written, read exactly rather
        // than found by scanning and trusting a record's position.
        Ok(pair
            .own
            .read_grant(identity, peer)
            .await?
            .granted()
            .map(|(grant, _ticket)| grant))
    }
}

/// Keep hosted `identity`'s connections bound: one sweep now, then one per
/// directory change and one per sweep interval. Without it a linked device
/// would refuse grants its identity issued until its first grant read, and
/// stay invisible to the counterparty. Holds the state weakly and upgrades
/// per sweep, so the task ends with the runtime instead of keeping it alive.
pub(crate) fn spawn_connection_armer(
    state: Weak<Mutex<State>>,
    identity: PdnId,
    changes: impl Stream<Item = Result<()>> + Send + Unpin + 'static,
) {
    let mut changes = changes;
    let _detached = tokio::spawn(async move {
        loop {
            let interval = {
                let Some(strong) = state.upgrade() else {
                    return;
                };
                let mut guard = strong.lock().await;
                arm_connections(&mut guard, identity, &state).await;
                guard.sweep_interval
            };
            // Two wake sources: a sweep can fail for a reason the directory
            // knows nothing about (an import that met a full disk), and
            // nothing then writes to the directory to ask for another try.
            tokio::select! {
                change = changes.next() => match change {
                    Some(Ok(())) => {}
                    Some(Err(_)) | None => return,
                },
                () = tokio::time::sleep(interval) => {}
            }
        }
    });
}

/// Keep the namespaces behind `(identity, peer)`'s live grants imported:
/// one sweep now, then one per change of the counterparty's replica —
/// payload events included, since a grant's ticket is a blob. The binder
/// owns exactly what it imported ([`State::bound_grants`]); a namespace
/// that arrived any other way is never touched. Exits, releasing its slot
/// in [`State::grant_binders`], when its replica is superseded.
pub(crate) fn spawn_grant_binder(
    state: Weak<Mutex<State>>,
    identity: PdnId,
    peer: PdnId,
    peer_store: ConnectionMetadataStore,
) {
    let _detached = tokio::spawn(async move {
        let mut changes = match peer_store.changes().await {
            Ok(changes) => changes,
            Err(_unsubscribable) => return release_binder(&state, identity, peer).await,
        };
        loop {
            {
                let Some(strong) = state.upgrade() else {
                    return;
                };
                let mut guard = strong.lock().await;
                if !bind_grants(&mut guard, identity, peer, &peer_store).await {
                    // The successor starts against a replica that has not
                    // synced yet: an inherited memo would read as "granted
                    // then withdrawn" on its first sweep and drop the
                    // namespace no one withdrew.
                    guard.bound_grants.retain(
                        |(bound_identity, bound_peer, _issuer), _namespace| {
                            (*bound_identity, *bound_peer) != (identity, peer)
                        },
                    );
                    guard.grant_binders.remove(&(identity, peer));
                    return;
                }
            }
            match changes.next().await {
                Some(Ok(())) => {}
                Some(Err(_)) | None => return release_binder(&state, identity, peer).await,
            }
        }
    });
}

async fn release_binder(state: &Weak<Mutex<State>>, identity: PdnId, peer: PdnId) {
    let Some(strong) = state.upgrade() else {
        return;
    };
    strong.lock().await.grant_binders.remove(&(identity, peer));
}

/// One sweep over the counterparty's replica; `false` once the pair is gone
/// or re-opened onto a fresh replica. A sweep never fails as a whole — a
/// read that fails leaves that grant for the next change.
async fn bind_grants(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
    peer_store: &ConnectionMetadataStore,
) -> bool {
    match state.metadata_pairs.get(&(identity, peer)) {
        Some(pair) if pair.peer.namespace() == peer_store.namespace() => {}
        _ => return false,
    }
    let Ok(listed) = peer_store.list_grants().await else {
        return true;
    };
    // In its own store a counterparty speaks for itself alone: `publish_grant`
    // refuses any other issuer, so a record keyed by a third identity is the
    // counterparty's word over data that is not its own, and binding it would
    // displace that identity's real binding in this registry.
    let granted: Vec<PdnId> = listed
        .into_iter()
        .filter(|issuer| *issuer == peer)
        .collect();
    for issuer in &granted {
        let _cold_until_next_change = bind_one_grant(state, identity, peer, *issuer, peer_store)
            .await
            .is_ok();
    }
    unbind_withdrawn(state, identity, peer, &granted).await;
    // Markers that arrived before their namespace bound act now.
    apply_retractions(state, identity).await;
    true
}

/// Import the namespace behind one live grant, for the identity the grant
/// addresses. The record is read before the decision: the
/// ticket inside it says which replica, so a grant republished onto a
/// fresh store rebinds too.
async fn bind_one_grant(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
    issuer: PdnId,
    peer_store: &ConnectionMetadataStore,
) -> Result<()> {
    // Only a decoded capability naming this issuer and identity: the key is
    // the counterparty's word, and acting on it would let one counterparty
    // hand this node a device set to trust over a third identity's data.
    let Some((_cap, ticket)) = peer_store.read_grant(issuer, identity).await?.granted() else {
        return Ok(());
    };
    let namespace = ticket.capability.id();
    let ticket_contacts = ticket.contacts();
    let bound = (identity, peer, issuer);
    // The memo is an optimization, the registry the arbiter, both ways: an
    // issuer already resolving to this namespace is adopted, since each
    // import holds one more handle and the last unbind must find exactly
    // one; an issuer resolving to nothing is re-imported even when the memo
    // matches.
    let memo_current = state.bound_grants.get(&bound) == Some(&namespace);
    let registered = state.node.data_namespace_of(identity, issuer)?;
    if registered == Some(namespace) {
        if !memo_current {
            state.bound_grants.insert(bound, namespace);
        }
    } else if !memo_current || registered.is_none() {
        let _import = state
            .node
            .import_namespace_scoped(identity, issuer, ticket)
            .await?;
        state.bound_grants.insert(bound, namespace);
    } else {
        // An import that arrived another way owns the replica; re-importing
        // would have two owners displace each other sweep by sweep.
        tracing::debug!(%issuer, "grant defers to a namespace imported another way");
    }
    // Refreshed even when the binding is unchanged: the device set moves
    // independently of the grant. An empty read is not an empty set — a
    // pair whose records have not replicated reads like one that published
    // nothing — so both derived sets are left as they were until a device
    // appears.
    let devices = published_issuer_devices(state, identity, peer).await;
    if devices.is_empty() {
        return Ok(());
    }
    let _unbound_meanwhile = state
        .node
        .track_retraction_peers(identity, issuer, devices.clone());
    refresh_replica_contacts(state, identity, issuer, &ticket_contacts, &devices).await
}

/// The devices the issuing identity published in the one connection whose
/// grant bound this replica. Another counterparty's word never reaches it:
/// the replica is held for one identity under one connection.
async fn published_issuer_devices(state: &State, identity: PdnId, peer: PdnId) -> Vec<NodeId> {
    let Some(pair) = state.metadata_pairs.get(&(identity, peer)) else {
        return Vec::new();
    };
    match pair.peer.published_devices().await {
        Ok(published) => published,
        Err(err) => {
            tracing::debug!("the pair's published device set was unreadable: {err:#}");
            Vec::new()
        }
    }
}

/// Set the granted replica's whole contact list: the issuing identity's
/// published devices, this identity's own siblings, and the ticket's
/// addressing for the published devices it names — each paired with the
/// identity it is dialed as. Derived from the device records on every
/// sweep, never kept — a stored list would be a second source of truth,
/// the one that goes stale — so removal is re-derivation. A contact
/// derived from a record carries the endpoint id alone; the endpoint
/// resolves paths it has spoken to.
async fn refresh_replica_contacts(
    state: &mut State,
    identity: PdnId,
    issuer: PdnId,
    ticket_contacts: &[Contact],
    issuer_devices: &[NodeId],
) -> Result<()> {
    let own = state.node.node_id();
    let siblings: Vec<NodeId> = match state.hosted(identity) {
        Ok(hosted) => hosted.directory.list_devices().await?,
        Err(_not_hosted) => Vec::new(),
    };
    let mut covered: HashSet<[u8; 32]> = HashSet::new();
    covered.insert(*own.as_bytes());
    let mut contacts: Vec<Contact> = Vec::new();
    // The ticket's entries carry address detail beyond the endpoint id, and
    // stay only while the issuer still publishes the device they name.
    for contact in ticket_contacts {
        let id = *contact.addr.id.as_bytes();
        if issuer_devices.iter().any(|device| *device.as_bytes() == id) && covered.insert(id) {
            contacts.push(contact.clone());
        }
    }
    for (device, holder) in issuer_devices
        .iter()
        .map(|device| (device, issuer))
        .chain(siblings.iter().map(|device| (device, identity)))
    {
        if covered.insert(*device.as_bytes()) {
            contacts.push(Contact::new(
                EndpointAddr::new(EndpointId::from_bytes(device.as_bytes())?),
                holder_of(holder),
            ));
        }
    }
    if contacts.is_empty() {
        return Ok(());
    }
    state
        .node
        .set_namespace_contacts(identity, issuer, contacts)
}

async fn unbind_withdrawn(state: &mut State, identity: PdnId, peer: PdnId, live: &[PdnId]) {
    let withdrawn: Vec<PdnId> = state
        .bound_grants
        .keys()
        .filter(|(bound_identity, bound_peer, issuer)| {
            *bound_identity == identity && *bound_peer == peer && !live.contains(issuer)
        })
        .map(|(_identity, _peer, issuer)| *issuer)
        .collect();
    for issuer in withdrawn {
        // Forget first, prune second, and drop the memo only if the forget
        // went through: a failed forget keeps the retry, and markers pruned
        // ahead of it would take away the only thing that could re-arm
        // them. The replica belongs to the identity this grant addresses,
        // so the pair that was swept decides alone. Own against granted is
        // the pair of ids, never what else the node hosts (ADR-0013): the
        // identity's own namespace is registered under its own id, and
        // only a forget naming it on both sides could take it away.
        if issuer != identity {
            match state.node.forget_namespace(identity, issuer).await {
                Ok(()) => {}
                Err(err) if err.downcast_ref::<data_layer::UnknownIssuer>().is_some() => {}
                Err(err) => {
                    tracing::warn!(
                        %issuer,
                        "failed to forget a withdrawn grant's namespace: {err:#}"
                    );
                    continue;
                }
            }
        }
        if let Ok(hosted) = state.hosted(identity) {
            let _cold_until_next_sweep = hosted.directory.prune_retractions(issuer).await;
        }
        state.bound_grants.remove(&(identity, peer, issuer));
    }
}

/// One arming sweep: open every directory-listed pair not yet cached, and
/// put a grant binder on every open pair. A pair that cannot open stays
/// cold until the next sweep. Binders are keyed off the cache rather than
/// this sweep's own opening, because establishment fills the cache
/// directly.
async fn arm_connections(state: &mut State, identity: PdnId, runtime: &Weak<Mutex<State>>) {
    apply_retractions(state, identity).await;
    bind_data_namespace(state, identity).await;
    if let Err(err) = ensure_own_device_confirmed(state, identity).await {
        tracing::warn!(%identity, "confirming this device in the directory failed: {err:#}");
    }
    let peers = {
        let Ok(hosted) = state.hosted(identity) else {
            return;
        };
        match hosted.directory.list_connections().await {
            Ok(peers) => peers,
            Err(_directory_unreadable) => return,
        }
    };
    for peer in peers {
        if !state.metadata_pairs.contains_key(&(identity, peer)) {
            let _cold_until_next_sweep = open_pair(state, identity, peer).await;
        } else if let Some(pair) = state.metadata_pairs.get(&(identity, peer)).cloned() {
            // A device that joined either side after the pair was opened
            // enters the set here rather than at the next restart.
            if let Err(err) = point_pair_at_its_devices(state, identity, peer, &pair).await {
                tracing::warn!(%identity, %peer, "the pair kept the contacts it had: {err:#}");
            }
        }
        let Some(pair) = state.metadata_pairs.get(&(identity, peer)) else {
            continue;
        };
        let peer_store = pair.peer.clone();
        if state.grant_binders.insert((identity, peer)) {
            spawn_grant_binder(runtime.clone(), identity, peer, peer_store);
        }
    }
}

/// Write this node's device record when the confirmed set lacks it — the
/// repair for a link whose confirmation failed after its hosting was
/// recorded, and for a kill between the two. Because of this write, device
/// removal cannot be the absence of the record: it has to be a record of
/// its own that this write consults, and the runtime has none.
pub(crate) async fn ensure_own_device_confirmed(state: &mut State, identity: PdnId) -> Result<()> {
    let own_device = state.node.node_id();
    let hosted = state.hosted(identity)?;
    if hosted.directory.list_devices().await?.contains(&own_device) {
        return Ok(());
    }
    hosted.directory.confirm_device(own_device).await
}

/// The recovery half of the data-namespace binding: a restarted node holds
/// the replica and the `data` ticket while its registry starts empty. The
/// import is idempotent against a replica the store already holds.
async fn bind_data_namespace(state: &mut State, identity: PdnId) {
    match state.node.data_namespace_of(identity, identity) {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => return,
    }
    let Ok(hosted) = state.hosted(identity) else {
        return;
    };
    let Ok(Some(ticket)) = hosted
        .directory
        .get_ticket(crate::identity::DATA_TICKET_KIND)
        .await
    else {
        return;
    };
    if let Err(err) = state
        .node
        .import_namespace(identity, identity, ticket)
        .await
    {
        // Warn: an import that keeps failing leaves the identity hosted and
        // unreadable with nothing else to say so.
        tracing::warn!(%identity, "re-binding the data namespace failed: {err:#}");
    }
}

/// The metadata pair of `(identity, peer)`, resolved against the
/// directory's per-connection kinds; `metadata_pairs` is only a handle
/// cache, and a cached side is reused only while it still names the
/// replica the directory names — otherwise a pair cached before the
/// counterparty re-established onto a fresh replica would silently miss
/// every later grant. `None` when the directory has no complete pair and
/// nothing is cached.
async fn open_pair(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
) -> Result<Option<ConnectionMetadata>> {
    let directory = &state.hosted(identity)?.directory;
    let own_ticket = directory
        .get_ticket(&data_layer::own_ticket_kind(&peer))
        .await?;
    let peer_ticket = directory
        .get_ticket(&data_layer::peer_ticket_kind(&peer))
        .await?;
    let (Some(own_ticket), Some(peer_ticket)) = (own_ticket, peer_ticket) else {
        // An already-open pair keeps working rather than blinking out.
        return Ok(state.metadata_pairs.get(&(identity, peer)).cloned());
    };
    // Each side judged on its own: a superseded `peer` must not force a
    // re-import of a still-current `own`.
    let own_namespace = own_ticket.capability.id();
    let peer_namespace = peer_ticket.capability.id();
    let ticket_addressing = (own_ticket.contacts(), peer_ticket.contacts());
    let cached = state.metadata_pairs.get(&(identity, peer)).cloned();
    let reuses_own = matches!(&cached, Some(pair) if pair.own.namespace() == own_namespace);
    let reuses_peer = matches!(&cached, Some(pair) if pair.peer.namespace() == peer_namespace);
    let own = match &cached {
        Some(pair) if reuses_own => pair.own.clone(),
        _ => data_layer::ConnectionMetadataStore::import(&state.node, identity, own_ticket).await?,
    };
    let peer_store = match &cached {
        Some(pair) if reuses_peer => pair.peer.clone(),
        _ => match data_layer::ConnectionMetadataStore::import(&state.node, identity, peer_ticket)
            .await
        {
            Ok(store) => store,
            Err(err) => {
                forget_imported(
                    state,
                    identity,
                    (!reuses_own).then_some(own_namespace),
                    None,
                )
                .await;
                return Err(err);
            }
        },
    };
    let pair = ConnectionMetadata {
        own,
        peer: peer_store,
    };
    if let Err(err) = arm_open_pair(state, identity, peer, &pair, ticket_addressing).await {
        forget_imported(
            state,
            identity,
            (!reuses_own).then_some(own_namespace),
            (!reuses_peer).then_some(peer_namespace),
        )
        .await;
        return Err(err);
    }
    state.metadata_pairs.insert((identity, peer), pair.clone());
    Ok(Some(pair))
}

/// Forget the halves this attempt imported: a failed open leaves the pair
/// uncached, and without this the open handles accumulate per attempt. A
/// half taken from the cache is left alone.
async fn forget_imported(
    state: &State,
    identity: PdnId,
    own: Option<data_layer::NamespaceId>,
    peer: Option<data_layer::NamespaceId>,
) {
    for namespace in [own, peer].into_iter().flatten() {
        if let Err(err) = state.node.forget_doc(identity, namespace).await {
            tracing::warn!(%namespace, "a metadata half stayed open after a failed pair open: {err:#}");
        }
    }
}

/// Assert this device into `own` once and register the pair for session
/// classification. Pointing the halves at their devices is not part of the
/// contract: its failure leaves the import-time contacts and is logged.
async fn arm_open_pair(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
    pair: &ConnectionMetadata,
    ticket_addressing: (Vec<Contact>, Vec<Contact>),
) -> Result<()> {
    #[cfg(feature = "test-util")]
    if let Some(failures) = state.pair_arm_failures.as_mut() {
        *failures += 1;
        anyhow::bail!("arming the pair failed for test");
    }
    pair.own
        .ensure_device_published(state.node.node_id())
        .await?;
    state
        .node
        .host_connection(identity, peer, &pair.own, &pair.peer)?;
    if let Err(err) = point_pair_at(state, identity, peer, pair, ticket_addressing).await {
        tracing::warn!(%identity, %peer, "the pair kept its import-time contacts: {err:#}");
    }
    Ok(())
}

/// Point both halves of the pair at every device that holds them. A ticket
/// names only the devices of the side that minted it, so without this a
/// grant published from a device that is now gone stays on the audience's
/// device alone, where the issuer's surviving devices cannot reach it and
/// refuse the audience fail-closed. Safe because both sides read each half
/// whole (Invariant 3) and a counterparty can withhold a record but never
/// forge one.
async fn point_pair_at_its_devices(
    state: &State,
    identity: PdnId,
    peer: PdnId,
    pair: &ConnectionMetadata,
) -> Result<()> {
    let directory = &state.hosted(identity)?.directory;
    let own_nodes = directory
        .get_ticket(&data_layer::own_ticket_kind(&peer))
        .await?
        .map(|ticket| ticket.contacts())
        .unwrap_or_default();
    let peer_nodes = directory
        .get_ticket(&data_layer::peer_ticket_kind(&peer))
        .await?
        .map(|ticket| ticket.contacts())
        .unwrap_or_default();
    point_pair_at(state, identity, peer, pair, (own_nodes, peer_nodes)).await
}

/// [`point_pair_at_its_devices`] for a caller that already holds what the
/// tickets address.
async fn point_pair_at(
    state: &State,
    identity: PdnId,
    peer: PdnId,
    pair: &ConnectionMetadata,
    (own_nodes, peer_nodes): (Vec<Contact>, Vec<Contact>),
) -> Result<()> {
    let directory = &state.hosted(identity)?.directory;
    let siblings = directory.list_devices().await?;
    let counterparty = pair.peer.published_devices().await?;

    let own_device = state.node.node_id();
    // A device is dialed as the identity it belongs to: this node's
    // siblings act for `identity`, the counterparty's devices for `peer`.
    let mut holders = Vec::new();
    for (device, holder) in siblings
        .iter()
        .map(|device| (device, identity))
        .chain(counterparty.iter().map(|device| (device, peer)))
    {
        if *device == own_device {
            continue;
        }
        match EndpointId::from_bytes(device.as_bytes()) {
            Ok(id) => holders.push(Contact::new(EndpointAddr::new(id), holder_of(holder))),
            Err(err) => tracing::warn!(%device, "undialable device record: {err:#}"),
        }
    }
    for (namespace, nodes) in [
        (pair.own.namespace(), own_nodes),
        (pair.peer.namespace(), peer_nodes),
    ] {
        // This device is covered first: a ticket it minted names it, and
        // the endpoint refuses a path to itself.
        let mut seen: HashSet<[u8; 32]> = HashSet::from([*own_device.as_bytes()]);
        let mut contacts = Vec::new();
        for node in nodes {
            if seen.insert(*node.addr.id.as_bytes()) {
                contacts.push(node);
            }
        }
        for holder in &holders {
            if seen.insert(*holder.addr.id.as_bytes()) {
                contacts.push(holder.clone());
            }
        }
        state.node.set_doc_contacts(identity, namespace, contacts)?;
    }
    Ok(())
}
