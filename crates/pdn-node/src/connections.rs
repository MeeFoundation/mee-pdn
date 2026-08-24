//! The connections service: establish a hosted identity's connections,
//! list them, and carry grants over the connections' metadata pairs.

use std::{
    collections::HashSet,
    sync::Weak,
    time::{Duration, Instant},
};

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, ConnectionMetadata, ConnectionMetadataStore, DocTicket, EndpointAddr,
    EndpointId, GrantedClaim, ReadGrant, ShareMode,
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

/// A grant publication named a data issuer other than the granting
/// identity itself — refused: granting another identity's data is
/// delegation, and delegation is not expressible. The classifier scans the
/// connections of the *data issuer's* identity, so a grant recorded under
/// a different granting identity could never be honored — the publish
/// would succeed, replicate, and enforce as nothing, a silent no-op on
/// both sides.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "identity {identity} cannot grant data issued by {issuer}: granting another identity's data \
     is delegation, which is not expressible"
)]
pub struct DelegationUnsupported {
    /// The identity attempting the grant.
    pub identity: PdnId,
    /// The foreign data issuer it tried to grant.
    pub issuer: PdnId,
}

/// A grant publish or withdrawal named a peer `identity` has no connection
/// metadata pair toward — established with, or replicated in from another
/// of its devices. Downcast from the `anyhow::Error` of the connections
/// service's `publish_grant`/`withdraw_grant`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no connection metadata pair toward {peer}")]
pub struct PeerNotConnected {
    /// The identity the grant was attempted from.
    pub identity: PdnId,
    /// The peer no metadata pair addresses.
    pub peer: PdnId,
}

/// A grant read from a connected peer's metadata store: the capability
/// naming the granted claims, and the ticket whose mode matches the grant's
/// commands. Reading is an observation — the grant binder is what imports
/// the granted subset, outside the replica's gossip swarm.
#[derive(Debug, Clone)]
pub struct PeerGrant {
    /// The capability: issuer, audience, exact claims, commands.
    pub grant: ReadGrant,
    /// The replica's ticket — addressing and contacts; read-mode for
    /// read-only grants (no namespace secret), write-mode with write.
    pub ticket: DocTicket,
}

/// Establishing, listing, and granting over a hosted identity's
/// connections. Establishment (the pairing dialogue, ADR-0011) is the
/// producer of connections: a one-sided record without the exchanged
/// metadata pair would contradict the connection model, so no manual
/// recording is offered.
///
/// Grants ride the connection's metadata pair: publishing writes into the
/// identity's own store toward the peer, and both halves are readable —
/// the counterpart's for what the peer granted, the identity's own for
/// what it granted itself. Either read opens the pair from the directory's
/// tickets on demand, so linked devices reach pairs established elsewhere.
/// Reading a peer's grant hands back the ticket it carries; acting on it
/// is the grant binder's job, not the caller's. Reading the identity's own
/// hands back the capability alone. One grant record exists per granted
/// issuer — every grant is scoped by an exact claim set — so a
/// republication replaces the previous record and a withdrawal is one act.
///
/// Both reads report what is readable at the moment of the call and never
/// wait: a record whose payload has not arrived reads as no grant. Both
/// answer for the device they run on — on the device that published a
/// grant the answer says the record is here, never that it reached a
/// sibling or the peer, and neither says what the peer received. An empty
/// answer covers four states: no connection toward that peer, a pair whose
/// tickets have not replicated here yet, a record here whose payload
/// cannot be read yet, and nothing granted. It is never evidence that
/// nothing is shared.
///
/// Neither read is free of writes: opening a pair publishes this device's
/// record into it and registers the connection for session classification,
/// the same acts the connection armer performs on its sweep. "Reports what
/// is readable now" means the call does not wait and changes no grant.
#[allow(async_fn_in_trait)]
pub trait ConnectionsService {
    /// Mint an invite for hosted `identity`: a one-time secret pending on
    /// this runtime (default lifetime, unless `lifetime` overrides it) and
    /// the self-contained payload to show the counterparty. The payload
    /// carries no bearer material — no tickets, no identity proof.
    async fn invite(&self, identity: PdnId, lifetime: Option<Duration>) -> Result<InvitePayload>;

    /// Establish a connection for hosted `identity` from a scanned invite
    /// payload: dial the payload's address on the pairing ALPN and run the
    /// establishment dialogue. A payload version this runtime does not
    /// speak is refused before dialing ([`UnsupportedInviteVersion`]).
    async fn establish(&self, identity: PdnId, invite: InvitePayload) -> Result<()>;

    /// List the current connections of hosted `identity`.
    async fn list(&self, identity: PdnId) -> Result<Vec<PdnId>>;

    /// Withdraw the grant of `issuer`'s data store toward `peer` — one
    /// tombstone over the single record. The issuer
    /// must be the granting identity itself, as for publishing. The grantee
    /// side unbinds once the tombstone replicates, and the replica leaves
    /// with the last grant that binds it: what the binders imported they
    /// also forget, so withdrawal reaches the bytes and not only the
    /// classification — but a node hosting several granted audiences of one
    /// issuer holds one shared replica (ADR-0009), and it stays until the
    /// last of their grants is withdrawn.
    async fn withdraw_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()>;

    /// Publish a grant: `identity` grants `peer` read — and, per claim,
    /// write — on exactly `claims` of `issuer`'s data store. The issuer must
    /// be the granting identity itself — anything else is refused
    /// ([`DelegationUnsupported`]). Capability and ticket travel as one
    /// record (replacing any previous grant for this issuer); the ticket
    /// follows the grant's commands as a whole: no write on any claim → a
    /// read ticket (no namespace secret — the grantee cannot write at all),
    /// write on any claim → a write ticket (the namespace secret is what
    /// carries write authority over the wire; the ingest gate at the
    /// issuer's devices scopes it per claim, ADR-0008).
    async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<GrantedClaim>,
    ) -> Result<()>;

    /// Read the grants `peer` has published toward hosted `identity`, over
    /// the pair's counterpart half — capability and ticket together.
    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>>;

    /// Read the grants hosted `identity` has published toward `peer`, over
    /// the pair's own half — the capability alone: the caller issues the
    /// namespace the record addresses, so a ticket to it answers nothing.
    async fn read_own_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<ReadGrant>>;
}

/// The production [`ConnectionsService`], backed by the runtime's
/// `data-layer` stack.
#[derive(Clone, Copy)]
pub struct RuntimeConnectionsService<'rt> {
    runtime: &'rt Runtime,
}

impl<'rt> RuntimeConnectionsService<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Self {
        Self { runtime }
    }

    /// The devices among the reconciliation contacts of the pair's two
    /// halves — own first, peer second — as the pair's open derived them.
    /// Observation only, for scenarios that assert the derivation itself:
    /// what it exists for is a record that reaches a device no ticket names,
    /// and the engine's own recorded peers rescue that often enough to hide
    /// the derivation behind luck. Behind the `test-util` feature and absent
    /// from every product build. Empty lists when no pair is open.
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
        let devices = |contacts: Vec<EndpointAddr>| {
            contacts
                .iter()
                .map(|contact| NodeId::from_bytes(*contact.id.as_bytes()))
                .collect::<Vec<_>>()
        };
        Ok((
            devices(state.node.doc_contacts(pair.own.namespace())?),
            devices(state.node.doc_contacts(pair.peer.namespace())?),
        ))
    }

    /// Whether the grant binder of `(identity, peer)` holds an imported
    /// binding for `issuer` — the sweep's own record of what it brought in.
    /// Observation only, for scenarios that must order an assertion after
    /// the sweep that acted on a grant change instead of sleeping and
    /// guessing. Behind the `test-util` feature and absent from every
    /// product build.
    #[cfg(feature = "test-util")]
    pub async fn grant_bound(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> bool {
        let state = self.runtime.state.lock().await;
        state.bound_grants.contains_key(&(identity, peer, issuer))
    }

    /// Drop the binder's record of an imported grant, replica untouched —
    /// the in-memory bookkeeping a device restart clears, hand-made so a
    /// scenario can put the unbind decision in front of a pair whose grant
    /// is durable and readable while the bookkeeping says nothing. Behind
    /// the `test-util` feature and absent from every product build.
    #[cfg(feature = "test-util")]
    pub async fn clear_grant_memo(&self, identity: PdnId, peer: PdnId, issuer: PdnId) {
        let mut state = self.runtime.state.lock().await;
        state.bound_grants.remove(&(identity, peer, issuer));
    }

    /// The devices the counterparty of `(identity, peer)` publishes in its
    /// half of the metadata pair, read as the grant sweep reads them.
    /// Observation only, for scenarios that assert the union across bound
    /// pairs against one pair's own word — a device withdrawn in one pair
    /// demonstrably gone from that pair's reading while the union still
    /// carries it — instead of sleeping and guessing whether the tombstone
    /// arrived. An unopened pair reads as publishing nothing, the same
    /// absence the sweep sees. Behind the `test-util` feature and absent
    /// from every product build.
    #[cfg(feature = "test-util")]
    pub async fn published_devices_of(&self, identity: PdnId, peer: PdnId) -> Result<Vec<NodeId>> {
        let state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        match state.metadata_pairs.get(&(identity, peer)) {
            Some(open) => open.peer.published_devices().await,
            None => Ok(Vec::new()),
        }
    }

    /// Run one grant sweep of `(identity, peer)` now — the very act the
    /// binder performs on a change event, invoked synchronously so a
    /// scenario can order an assertion after a sweep that has demonstrably
    /// seen a given record state, instead of racing the event's delivery.
    /// Arrangement only; an unopened pair sweeps as nothing. Behind the
    /// `test-util` feature and absent from every product build.
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
        // The version refusal precedes the dial. The hosted check and every
        // other step run inside the dialogue, which takes the runtime lock
        // per phase and never holds it across the network round-trip — see
        // `establish_via_dialogue`.
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
        // The ticket follows the grant's commands as a whole.
        let mode = if grant.grants_any_write() {
            ShareMode::Write
        } else {
            ShareMode::Read
        };
        let ticket = state
            .node
            .share_ticket(issuer, mode, AddrInfoOptions::RelayAndAddresses)
            .await?;
        pair.own.publish_grant(&grant, &ticket).await
    }

    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>> {
        // The pair is resolved under the lock, the grant reads run outside it.
        let pair = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            open_pair(&mut state, identity, peer).await?
        };
        let Some(pair) = pair else {
            return Ok(Vec::new());
        };
        let mut grants = Vec::new();
        for issuer in pair.peer.list_grants().await? {
            if let Some((grant, ticket)) = pair.peer.read_grant(issuer, identity).await?.granted() {
                grants.push(PeerGrant { grant, ticket });
            }
        }
        Ok(grants)
    }

    async fn read_own_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<ReadGrant>> {
        let pair = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            open_pair(&mut state, identity, peer).await?
        };
        // No pair here: no connection toward `peer`, or its tickets have not
        // replicated to this device yet. Both answer as the peer-side read
        // does, and neither says anything is ungranted.
        let Some(pair) = pair else {
            return Ok(Vec::new());
        };
        let mut grants = Vec::new();
        for issuer in pair.own.list_grants().await? {
            // The ticket is the one minted for `peer` over this identity's
            // own namespace: nothing the issuer does not already hold.
            if let Some((grant, _ticket)) = pair.own.read_grant(issuer, peer).await?.granted() {
                grants.push(grant);
            }
        }
        Ok(grants)
    }
}

/// Keep hosted `identity`'s connections bound for session classification:
/// one sweep now, then one per directory change and one per sweep
/// interval, each opening every directory-listed pair not yet cached.
/// Hosting an identity thereby keeps its pairs registered — on the device
/// that established them and on every linked device their records
/// replicate to. Without the sweep a linked
/// device would silently refuse grants its identity really issued until
/// its first grant read, and stay invisible to the counterparty
/// ([`open_pair`] is also what publishes the device record).
///
/// The task holds the runtime state weakly and upgrades per sweep, so
/// shutdown's sole-ownership wait is never blocked for longer than one
/// sweep's local acts; it exits when the runtime is gone or the directory's
/// event stream ends with it.
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
            // Two wake sources, not one. A sweep can fail for a reason the
            // directory knows nothing about — an import that met a full
            // disk — and nothing then writes to the directory to ask for
            // another try, so a change-driven armer alone would leave the
            // identity unbound until something unrelated happened to write
            // there. The timer bounds that to one interval.
            tokio::select! {
                change = changes.next() => match change {
                    Some(Ok(())) => {}
                    // A failed subscription or a stream ended by shutdown
                    // both end the armer; the grant surface's on-demand
                    // open remains as the backstop.
                    Some(Err(_)) | None => return,
                },
                () = tokio::time::sleep(interval) => {}
            }
        }
    });
}

/// Keep the data namespaces behind `(identity, peer)`'s live grants
/// imported, so a granted store arrives without an explicit act: one sweep
/// now, then one per change of the counterparty's metadata replica. The
/// replica's payload events are part of that trigger because a grant's
/// ticket is a blob — the record arriving is not yet the ticket being
/// readable, and only the payload event says to look again.
///
/// The binder owns exactly what it imported ([`State::bound_grants`]): a
/// grant that appears is imported, a grant whose ticket comes to name a
/// different replica is re-imported onto it, and a grant that disappears is
/// unbound again — the shared replica itself leaves only with the last pair
/// bound to it. A namespace that arrived any other way is never touched, so
/// an out-of-band import is not dropped from under its owner.
///
/// Like the connection armer the task holds the state weakly and upgrades
/// per sweep. It exits when the runtime is gone, when the event stream ends
/// with the node, or when its replica is superseded — releasing its slot in
/// [`State::grant_binders`] on the way out, so the next connection sweep
/// spawns a fresh binder against the replica that replaced it.
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
                    // Hand the pair over, bookkeeping included: the successor
                    // starts against a replica that has not synced yet, and
                    // an inherited record of what was imported would read as
                    // "granted then withdrawn" on its first sweep and drop
                    // the namespace no one withdrew. It re-imports instead —
                    // idempotent when the grant is still there.
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

/// Give up this pair's binder slot, so a later sweep can spawn a new one.
async fn release_binder(state: &Weak<Mutex<State>>, identity: PdnId, peer: PdnId) {
    let Some(strong) = state.upgrade() else {
        return;
    };
    strong.lock().await.grant_binders.remove(&(identity, peer));
}

/// One grant-arming sweep over the counterparty's replica. Answers whether
/// this binder is still the right one to watch it: `false` once the pair is
/// gone or re-opened onto a fresh replica, which ends the task.
///
/// A sweep never fails as a whole — a read that fails leaves that grant for
/// the next change rather than abandoning the ones beside it.
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
    let Ok(granted) = peer_store.list_grants().await else {
        return true;
    };
    for issuer in &granted {
        let _cold_until_next_change = bind_one_grant(state, identity, peer, *issuer, peer_store)
            .await
            .is_ok();
    }
    unbind_withdrawn(state, identity, peer, &granted).await;
    // Markers that arrived before their namespace bound act on the sweep
    // after the binding — this one.
    apply_retractions(state, identity).await;
    true
}

/// Import the data namespace behind one live grant, unless this binder
/// already holds exactly the replica the grant names. The record is read
/// before that decision, not after: the ticket inside it is what says which
/// replica, so a grant republished onto a fresh store rebinds here too.
async fn bind_one_grant(
    state: &mut State,
    identity: PdnId,
    peer: PdnId,
    issuer: PdnId,
    peer_store: &ConnectionMetadataStore,
) -> Result<()> {
    // No record, a payload still replicating, a record kind this build cannot
    // decode, or a capability addressed to another issuer or another identity:
    // nothing to act on until the next change. The capability is what says
    // whose data this is — the key it was written under is the counterparty's
    // word, and acting on it would let one counterparty hand this node a
    // device set to trust over a third identity's namespace.
    let Some((_cap, ticket)) = peer_store.read_grant(issuer, identity).await?.granted() else {
        return Ok(());
    };
    let namespace = ticket.capability.id();
    let ticket_nodes = ticket.nodes.clone();
    let bound = (identity, peer, issuer);
    // The memo is an optimization, the registry the arbiter — in both
    // directions. An issuer already resolving to the very namespace the
    // grant names (another pair's binder imported the shared replica,
    // ADR-0009) is adopted into the memo rather than re-imported: each
    // import holds one more open handle on the replica, and the last
    // unbind's forget must find exactly one to drop. An issuer resolving to
    // nothing — forgotten while the memo survived — is re-imported even
    // when the memo matches, so a desync between the two heals on this
    // sweep instead of skipping the import forever.
    let memo_current = state.bound_grants.get(&bound) == Some(&namespace);
    let registered = state.node.data_namespace_of(issuer)?;
    if registered == Some(namespace) {
        if !memo_current {
            state.bound_grants.insert(bound, namespace);
        }
    } else if !memo_current || registered.is_none() {
        let _displaced = state.node.import_namespace_scoped(issuer, ticket).await?;
        state.bound_grants.insert(bound, namespace);
    } else {
        // The issuer resolves to a replica this grant does not name: an
        // import that arrived another way owns it, and the binder defers —
        // re-importing here would have two owners displace each other sweep
        // by sweep, each opening one more replica handle. The deferral ends
        // when that import is forgotten or the grant renames its replica;
        // logged so the waiting state is diagnosable.
        tracing::debug!(%issuer, "grant defers to a namespace imported another way");
    }
    // The provisional-write tracker and the replica's contacts both follow
    // the issuer's published device set — refreshed even when the binding is
    // unchanged: the set moves independently of the grant.
    //
    // An empty read is not an empty set. A pair whose `devices/` records
    // have not replicated yet reads exactly like a counterparty that
    // published nothing, so an empty union is "nobody has spoken yet", and
    // taking it as the set would leave the tracker honoring no rejection at
    // all — a provisional write that never retracts — and the contact set
    // stripped of every route to the issuer. Both are left as they were
    // until a device appears.
    let bound_pairs = pairs_bound_to(state, issuer);
    let devices = published_issuer_devices(state, &bound_pairs).await;
    if devices.is_empty() {
        return Ok(());
    }
    let _unbound_meanwhile = state.node.track_retraction_peers(issuer, devices.clone());
    refresh_replica_contacts(state, issuer, &ticket_nodes, &devices, &bound_pairs).await
}

/// The pairs whose grant binds `issuer` here, as `(audience, counterparty)`.
/// One namespace per issuer (ADR-0009) means several hosted audiences share
/// one replica, so everything derived for that replica is derived across all
/// of them: the tracked contact set and the retraction tracker's device set
/// are both per replica, and deriving either from one pair would have each
/// binder's sweep replace what the others established.
fn pairs_bound_to(state: &State, issuer: PdnId) -> Vec<(PdnId, PdnId)> {
    state
        .bound_grants
        .keys()
        .filter(|(_identity, _peer, bound_issuer)| *bound_issuer == issuer)
        .map(|(identity, peer, _issuer)| (*identity, *peer))
        .collect()
}

/// The issuer's devices as every bound pair states them, unioned. The pairs
/// replicate independently, so their views differ while one lags, and a pair
/// whose records have not arrived reads exactly like one that published
/// nothing — it contributes nothing here and the others still speak, where
/// one pair's word taken alone would strip a device the issuer never
/// withdrew. A device therefore leaves the union only once no bound pair
/// publishes it.
async fn published_issuer_devices(state: &State, bound_pairs: &[(PdnId, PdnId)]) -> Vec<NodeId> {
    let stores: Vec<ConnectionMetadataStore> = bound_pairs
        .iter()
        .filter_map(|pair| state.metadata_pairs.get(pair).map(|open| open.peer.clone()))
        .collect();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut devices = Vec::new();
    for store in stores {
        // Unreadable is indistinguishable from not-yet-replicated, and both
        // are "this pair says nothing this sweep" — logged, so a pair that
        // stays unreadable is diagnosable.
        let published = match store.published_devices().await {
            Ok(published) => published,
            Err(err) => {
                tracing::debug!("skipped an unreadable pair in the device union: {err:#}");
                continue;
            }
        };
        for device in published {
            if seen.insert(*device.as_bytes()) {
                devices.push(device);
            }
        }
    }
    devices
}

/// Point the granted replica at every device authorized to serve it: the
/// issuer's published devices — the same set the retraction tracker
/// follows, unioned over every pair whose grant binds this issuer — and the
/// audience siblings, with the grant ticket's addressing kept for the
/// published devices it names. Derived whole from the device records on
/// every sweep and set as the replica's entire contact list, never kept:
/// the records are the durable truth and they replicate, so a list stored
/// beside them would be a second source of truth for one fact — and the one
/// that goes stale. Removal is re-derivation — a device withdrawn on either
/// side stops appearing in its record set, so the next sweep's contact list
/// simply lacks it, the publishing device included.
///
/// The siblings, like the issuer devices, are those of *every* hosted
/// identity whose grant binds this issuer, not only the sweeping one: the
/// replica — and its tracked contact set — is per issuer, so a per-identity
/// set would have each binder's sweep strip the other identity's siblings.
///
/// A contact derived from a device record carries the endpoint id alone:
/// the endpoint resolves paths it has spoken to — the audience's siblings
/// sync with this node's device-shared stores, and the issuer's devices
/// dial it to serve the connection's metadata pair.
async fn refresh_replica_contacts(
    state: &mut State,
    issuer: PdnId,
    ticket_nodes: &[EndpointAddr],
    issuer_devices: &[NodeId],
    bound_pairs: &[(PdnId, PdnId)],
) -> Result<()> {
    let own = state.node.node_id();
    let mut siblings: Vec<NodeId> = Vec::new();
    for (audience, _peer) in bound_pairs {
        if let Ok(hosted) = state.hosted(*audience) {
            siblings.extend(hosted.directory.list_devices().await?);
        } else {
            // A bound pair whose audience is not hosted — the unlink
            // window; its siblings leave the set by this very derivation.
            tracing::debug!(%audience, "skipped a bound pair whose audience is not hosted");
        }
    }
    let mut covered: HashSet<[u8; 32]> = HashSet::new();
    covered.insert(*own.as_bytes());
    let mut contacts = Vec::new();
    // The ticket's entries first: they carry address detail beyond the
    // endpoint id, and stay only while the issuer still publishes the
    // device they name.
    for node in ticket_nodes {
        let id = *node.id.as_bytes();
        if issuer_devices.iter().any(|device| *device.as_bytes() == id) && covered.insert(id) {
            contacts.push(node.clone());
        }
    }
    for device in issuer_devices.iter().chain(siblings.iter()) {
        if covered.insert(*device.as_bytes()) {
            contacts.push(EndpointAddr::new(EndpointId::from_bytes(
                device.as_bytes(),
            )?));
        }
    }
    if contacts.is_empty() {
        return Ok(());
    }
    state.node.set_namespace_contacts(issuer, contacts)
}

/// Whether any pair other than `unbinding` still holds `issuer`'s replica:
/// a binding another binder memoized, or a live readable grant record in an
/// open pair's counterparty replica. The memo alone would undercount — it
/// is in-memory bookkeeping, rebuilt sweep by sweep, and a pair whose
/// binder has not swept since holds its grant durably while the memo says
/// nothing (operating-conditions: the device restarts) — so the decision
/// that destroys a replica consults the grant records too. `Unreadable`
/// does not hold: a record whose payload never resolves would pin the
/// replica forever, and a pair whose live grant goes uncounted re-imports
/// on its next sweep ([`bind_one_grant`]'s registration check), refetching
/// the bytes over the network.
async fn held_by_another_pair(state: &State, unbinding: (PdnId, PdnId), issuer: PdnId) -> bool {
    let memoized = state
        .bound_grants
        .keys()
        .any(|(identity, peer, bound)| *bound == issuer && (*identity, *peer) != unbinding);
    if memoized {
        return true;
    }
    for ((audience, peer), open) in &state.metadata_pairs {
        if (*audience, *peer) == unbinding {
            continue;
        }
        // Unreadable or unreachable is no evidence of holding either way;
        // a grant that turns out live re-imports on its own sweep.
        if let Ok(data_layer::GrantRead::Granted(..)) =
            open.peer.read_grant(issuer, *audience).await
        {
            return true;
        }
    }
    false
}

/// Unbind the namespaces whose grant this pair no longer carries — the
/// counterpart of the import above, bounded to what this binder brought in.
/// The replica itself leaves with the last bound pair: one namespace per
/// issuer (ADR-0009) means every pair granted by this issuer binds the same
/// replica, and forgetting it while another pair's grant stands would take
/// the bytes from an audience nobody withdrew. A republished grant imports
/// afresh, so dropping the replica is not a one-way door for the
/// connection, only for the bytes held under the last withdrawn grant.
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
        // Forget first, prune second, and drop the binding only if the forget
        // went through (or was skipped for a surviving holder). Forgetting is
        // what unregisters the issuer, disarms its retractions and untracks
        // it; a failed forget leaves all three standing, and markers pruned
        // ahead of it would have taken away the only thing that could explain
        // or re-arm them. Keeping the binding keeps the retry: the next sweep
        // sees the grant still gone.
        // A hosted identity's own namespace predates every grant binding and
        // outlives them all — the last grant toward it disappearing must
        // never destroy it, same as `linking.rs`'s rollback never destroys a
        // namespace an import displaced rather than created.
        if !state.is_hosted(issuer) && !held_by_another_pair(state, (identity, peer), issuer).await
        {
            match state.node.forget_namespace(issuer).await {
                // Forgotten here, or already resolving to nothing — the state
                // this unbind drives at.
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
        // Retraction markers leave with the grant binding: this identity's
        // verdicts have no grant to act under, whether the replica stayed
        // with another pair or left with this one.
        if let Ok(hosted) = state.hosted(identity) {
            let _cold_until_next_sweep = hosted.directory.prune_retractions(issuer).await;
        }
        state.bound_grants.remove(&(identity, peer, issuer));
    }
}

/// One arming sweep: open every pair `identity`'s directory lists that is
/// not in the cache yet, and put a grant binder on every pair that is open.
/// A pair that cannot open — its tickets still payload-waiting, or a
/// transient store failure — stays cold until the next sweep; a sweep never
/// fails as a whole.
///
/// Arming grants is keyed off the cache rather than off this sweep's own
/// opening, because establishment fills the cache directly: a pair this
/// device paired itself would otherwise never be watched for grants.
async fn arm_connections(state: &mut State, identity: PdnId, runtime: &Weak<Mutex<State>>) {
    // Every directory change re-applies the retraction markers: a marker
    // replicated from a sibling acts here, and re-application is idempotent.
    apply_retractions(state, identity).await;
    // The identity's data namespace follows the directory's `data` ticket:
    // bound already on the device that created or linked the identity, and
    // re-bound here when the node holds the directory alone — a restart. A
    // ticket whose payload has not landed yet leaves the issuer unbound
    // until a later sweep; a read meanwhile refuses as unknown, fail-closed.
    bind_data_namespace(state, identity).await;
    // The identity's own device record, written whenever the directory
    // lacks it: a device writes it after its hosting is durable, never
    // before, so a start that finds itself hosted and unnamed writes it
    // here — and so does a link whose confirmation did not go through.
    // Until it lands, the identity's other devices refuse this one.
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
            // Device records replicate, so a device that joined either side
            // after this pair was opened enters the set here rather than at
            // the next restart.
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

/// Write this node's device record into hosted `identity`'s directory when
/// the confirmed set does not name it. Idempotent by construction — the
/// read decides — and the repair for both windows that leave a hosted
/// device unnamed: a link whose confirmation failed after its hosting was
/// recorded, and a process killed between the two.
///
/// Removal of a device cannot be expressed as the absence of this record,
/// precisely because of this write: an absence would be undone at the next
/// sweep, with nothing left to say a removal ever happened. Nothing here
/// reads a revocation record because the runtime has none; what keeps this
/// safe is that removal, when it exists, is a record of its own that this
/// write consults ([device-linking](../../../mia-docs/openspec/specs/components/pdn-node/device-linking.md)).
pub(crate) async fn ensure_own_device_confirmed(state: &mut State, identity: PdnId) -> Result<()> {
    let own_device = state.node.node_id();
    let hosted = state.hosted(identity)?;
    if hosted.directory.list_devices().await?.contains(&own_device) {
        return Ok(());
    }
    hosted.directory.confirm_device(own_device).await
}

/// Bind hosted `identity`'s data namespace from its directory's `data`
/// ticket when nothing is bound for it — the recovery half of the binding:
/// creation and linking bind it themselves, and a restarted node holds the
/// replica and the ticket while its in-memory registry starts empty. The
/// import is idempotent against a replica the store already holds (the
/// capability merges), so re-running it on every sweep converges. Nothing
/// here dials by itself; the import's sync attempts ride the ordinary
/// reconcile machinery.
async fn bind_data_namespace(state: &mut State, identity: PdnId) {
    match state.node.data_namespace_of(identity) {
        Ok(None) => {}
        // Bound already, or a registry failure this sweep cannot judge —
        // the next sweep retries.
        Ok(Some(_)) | Err(_) => return,
    }
    let Ok(hosted) = state.hosted(identity) else {
        return;
    };
    // Absent, payload-waiting, or unreadable: cold until the payload's
    // arrival fires the next sweep.
    let Ok(Some(ticket)) = hosted
        .directory
        .get_ticket(crate::identity::DATA_TICKET_KIND)
        .await
    else {
        return;
    };
    if let Err(err) = state.node.import_namespace(identity, ticket).await {
        // Warn, not debug: this is the identity's own data namespace, and
        // an import that keeps failing leaves it hosted and unreadable
        // with nothing else to say so. The retry is the next sweep.
        tracing::warn!(%identity, "re-binding the data namespace failed: {err:#}");
    }
}

/// The metadata pair of `(identity, peer)`, resolved against the directory's
/// per-connection kinds — the durable truth for which replicas the pair
/// addresses; `metadata_pairs` is only a handle cache. Both tickets are read
/// every time and a cached side is reused *only while it still names the
/// replica the directory names*: a pair cached before the counterparty
/// re-established onto a fresh replica would otherwise keep reading the
/// superseded one and silently miss every later grant. The re-read is two
/// local replica reads — no network — and buys that staleness check.
///
/// Tickets are imported on demand, so a linked device opens pairs
/// established on the identity's other devices. `None` when the directory
/// has no complete pair for `peer` (not connected, or the tickets have not
/// synced here yet) and nothing is cached.
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
        // No complete pair in the directory: not connected, or a kind — or
        // its payload — has not synced here yet. An already-open pair keeps
        // working meanwhile rather than blinking out.
        return Ok(state.metadata_pairs.get(&(identity, peer)).cloned());
    };
    // Each side is judged on its own, so a superseded `peer` does not force a
    // needless re-import of a still-current `own` (which would leak a tracked
    // doc and an author).
    let own_namespace = own_ticket.capability.id();
    let peer_namespace = peer_ticket.capability.id();
    let cached = state.metadata_pairs.get(&(identity, peer)).cloned();
    let own = match &cached {
        Some(pair) if pair.own.namespace() == own_namespace => pair.own.clone(),
        _ => data_layer::ConnectionMetadataStore::import(&state.node, own_ticket).await?,
    };
    let peer_store = match &cached {
        Some(pair) if pair.peer.namespace() == peer_namespace => pair.peer.clone(),
        _ => data_layer::ConnectionMetadataStore::import(&state.node, peer_ticket).await?,
    };
    let pair = ConnectionMetadata {
        own,
        peer: peer_store,
    };
    // A device that opens the pair asserts itself into `own` once and
    // registers the pair for session classification, so linked devices are
    // resolvable by the counterparty and judge callers themselves.
    // Assert-once, tombstones respected: opening is not a (re-)publication
    // act — a live record is left untouched, and a withdrawn record is
    // never resurrected as a side effect.
    pair.own
        .ensure_device_published(state.node.node_id())
        .await?;
    state
        .node
        .host_connection(identity, peer, &pair.own, &pair.peer)?;
    if let Err(err) = point_pair_at_its_devices(state, identity, peer, &pair).await {
        tracing::warn!(%identity, %peer, "the pair kept its import-time contacts: {err:#}");
    }
    state.metadata_pairs.insert((identity, peer), pair.clone());
    Ok(Some(pair))
}

/// Point both halves of a connection's metadata pair at every device that
/// holds them — the identity's own and the peer's — beside the addressing
/// each ticket carries. A ticket names the devices of the side that minted
/// it alone, so a half whose minting side is offline has nowhere to
/// reconcile with: a grant one of this identity's other devices already
/// holds never crosses, and a grant published from a device that is now
/// gone stays on the audience's device alone, where the issuer's surviving
/// devices cannot reach it and refuse the audience fail-closed.
///
/// Both sides read each half whole (Invariant 3), so a device of either can
/// serve it and neither gains anything by it: every entry is signed by its
/// author and its payload addressed by its hash, so a counterparty can
/// withhold a record but never forge one.
///
/// Derived from the durable device records whenever the pair is opened,
/// never stored: the records are the durable truth and they replicate.
/// A half whose doc is not tracked keeps whatever contacts it has: the pair
/// is open and usable, and the next sweep to open it derives the set again.
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
        .map(|ticket| ticket.nodes)
        .unwrap_or_default();
    let peer_nodes = directory
        .get_ticket(&data_layer::peer_ticket_kind(&peer))
        .await?
        .map(|ticket| ticket.nodes)
        .unwrap_or_default();
    let mut devices = directory.list_devices().await?;
    devices.extend(pair.peer.published_devices().await?);

    let own_device = state.node.node_id();
    let mut holders = Vec::new();
    for device in devices.iter().filter(|device| **device != own_device) {
        match EndpointId::from_bytes(device.as_bytes()) {
            Ok(id) => holders.push(EndpointAddr::new(id)),
            Err(err) => tracing::warn!(%device, "undialable device record: {err:#}"),
        }
    }
    for (namespace, nodes) in [
        (pair.own.namespace(), own_nodes),
        (pair.peer.namespace(), peer_nodes),
    ] {
        // This device is covered before anything is taken in, so a ticket
        // that names the side that minted it — this one, for the half it
        // minted — leaves no contact pointing here: the endpoint refuses a
        // path to itself, and the dial is spent once per reconcile.
        let mut seen: HashSet<[u8; 32]> = HashSet::from([*own_device.as_bytes()]);
        let mut contacts = Vec::new();
        for node in nodes {
            if seen.insert(*node.id.as_bytes()) {
                contacts.push(node);
            }
        }
        for holder in &holders {
            if seen.insert(*holder.id.as_bytes()) {
                contacts.push(holder.clone());
            }
        }
        state.node.set_doc_contacts(namespace, contacts)?;
    }
    Ok(())
}
