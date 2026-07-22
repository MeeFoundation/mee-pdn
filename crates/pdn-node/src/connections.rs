//! The connections service: establish a hosted identity's connections,
//! list them, and carry grants over the connections' metadata pairs.

use std::sync::Weak;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use data_layer::{
    AddrInfoOptions, ConnectionMetadata, ConnectionMetadataStore, DocTicket, EndpointAddr,
    EndpointId, ReadGrant, ShareMode,
};
use futures_lite::{Stream, StreamExt};
use pdn_types::{ClaimId, NonEmpty, PdnId};
use tokio::sync::Mutex;

use crate::pairing::{
    establish_via_dialogue, InvitePayload, UnsupportedInviteVersion, DEFAULT_INVITE_LIFETIME,
    INVITE_FORMAT_VERSION,
};
use crate::runtime::{Runtime, State};

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
/// identity's own store toward the peer, reading opens the counterpart's
/// store — from the directory's tickets on demand, so linked devices reach
/// pairs established elsewhere. Reading hands back the ticket a grant
/// carries; acting on it is the grant binder's job, not the caller's. One
/// grant record exists per granted issuer — every grant is scoped by an
/// exact claim set — so a republication replaces the previous record and a
/// withdrawal is one act.
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
    /// side drops the namespace once the tombstone replicates: what its
    /// grant binder imported it also forgets, so withdrawal reaches the
    /// bytes and not only the classification.
    async fn withdraw_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()>;

    /// Publish a grant: `identity` grants `peer` read — and, with `write`,
    /// write — on exactly `claims` of `issuer`'s data store. The issuer must
    /// be the granting identity itself — anything else is refused
    /// ([`DelegationUnsupported`]). Capability and ticket travel as one
    /// record (replacing any previous grant for this issuer); the ticket
    /// carries exactly the granted authority: read-only → a read ticket (no
    /// namespace secret — the grantee cannot write at all), with `write` →
    /// a write ticket (the namespace secret carries write authority — no
    /// ingest hook is installed, ADR-0008).
    async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<ClaimId>,
        write: bool,
    ) -> Result<()>;

    /// Read the grants `peer` has published toward hosted `identity` —
    /// capability and ticket together, with the same
    /// payload-waiting and poll-friendly contract as
    /// [`read_grants`](Self::read_grants).
    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>>;
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
            .with_context(|| format!("no connection metadata pair toward {peer}"))?;
        pair.own.withdraw_grant(issuer).await
    }

    async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<ClaimId>,
        write: bool,
    ) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        if identity != issuer {
            return Err(DelegationUnsupported { identity, issuer }.into());
        }
        let pair = open_pair(&mut state, identity, peer)
            .await?
            .with_context(|| format!("no connection metadata pair toward {peer}"))?;
        // The ticket carries exactly the granted authority.
        let mode = if write {
            ShareMode::Write
        } else {
            ShareMode::Read
        };
        let ticket = state
            .node
            .share_ticket(issuer, mode, AddrInfoOptions::RelayAndAddresses)
            .await?;
        let grant = ReadGrant {
            issuer,
            audience: peer,
            claims,
            write,
        };
        pair.own.publish_grant(&grant, &ticket).await
    }

    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>> {
        // Assembly under the lock, polling outside it, exactly as
        // `read_grants`.
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
            if let Some((grant, ticket)) = pair.peer.read_grant(issuer).await? {
                grants.push(PeerGrant { grant, ticket });
            }
        }
        Ok(grants)
    }
}

/// Keep hosted `identity`'s connections bound for session classification:
/// one sweep now, then one per directory change, each opening every
/// directory-listed pair not yet cached. Hosting an identity thereby keeps
/// its pairs registered — on the device that established them and on every
/// linked device their records replicate to. Without the sweep a linked
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
            {
                let Some(strong) = state.upgrade() else {
                    return;
                };
                let mut guard = strong.lock().await;
                arm_connections(&mut guard, identity, &state).await;
            }
            match changes.next().await {
                Some(Ok(())) => {}
                // A failed subscription or a stream ended by shutdown both
                // end the armer; the grant surface's on-demand open remains
                // as the backstop.
                Some(Err(_)) | None => return,
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
/// forgotten again. A namespace that arrived any other way is never
/// touched, so an out-of-band import is not dropped from under its owner.
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
    // No record, a payload still replicating, or a record kind this build
    // cannot decode: nothing to act on until the next change.
    let Some((_cap, ticket)) = peer_store.read_grant(issuer).await? else {
        return Ok(());
    };
    let namespace = ticket.capability.id();
    let bound = (identity, peer, issuer);
    if state.bound_grants.get(&bound) != Some(&namespace) {
        let _displaced = state.node.import_namespace_scoped(issuer, ticket).await?;
        state.bound_grants.insert(bound, namespace);
    }
    // Contacts are refreshed even when the binding is unchanged: the
    // audience's device set moves independently of the grant.
    refresh_sibling_contacts(state, identity, issuer).await
}

/// Point the granted replica at the audience identity's other devices, so
/// it catches up from a sibling while the issuer is away. Derived from the
/// directory on every sweep rather than kept: the `devices/` records are
/// the durable truth and they replicate, so a list stored beside them would
/// be a second source of truth for one fact — and the one that goes stale.
///
/// A contact carries the endpoint id alone: the endpoint resolves paths it
/// has spoken to, so a sibling this node has synced with is dialable and
/// one it never reached stays inert until it is.
async fn refresh_sibling_contacts(state: &mut State, identity: PdnId, issuer: PdnId) -> Result<()> {
    let own = state.node.node_id();
    let devices = state.hosted(identity)?.directory.list_devices().await?;
    let mut contacts = Vec::new();
    for device in devices {
        if device == own {
            continue;
        }
        contacts.push(EndpointAddr::new(EndpointId::from_bytes(
            device.as_bytes(),
        )?));
    }
    if contacts.is_empty() {
        return Ok(());
    }
    state.node.add_namespace_contacts(issuer, contacts)
}

/// Forget the namespaces whose grant this pair no longer carries — the
/// counterpart of the import above, bounded to what this binder brought in.
/// A republished grant imports afresh, so dropping the replica is not a
/// one-way door for the connection, only for the bytes held under a grant
/// that no longer exists.
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
        let _gone_or_already_gone = state.node.forget_namespace(issuer).await;
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
    state.metadata_pairs.insert((identity, peer), pair.clone());
    Ok(Some(pair))
}
