//! The connections service: establish a hosted identity's connections,
//! list them, and carry grants over the connections' metadata pairs.

use std::sync::Weak;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use data_layer::{AddrInfoOptions, ConnectionMetadata, DocTicket, ReadGrant, ShareMode};
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

/// A grant read from a connected peer's metadata store: the peer granted
/// access to the data store `issuer` issues, and the ticket is the
/// whole-store payload — importing it (an explicit data-service act) is how
/// the granted namespace starts syncing here.
#[derive(Debug, Clone)]
pub struct PeerGrant {
    /// The issuer of the granted data store.
    pub issuer: PdnId,
    /// The whole-store ticket the grant carries.
    pub ticket: DocTicket,
}

/// A scoped grant read from a connected peer's metadata store: the
/// capability naming the granted claims, and the ticket whose mode matches
/// the grant's commands. Importing the namespace scoped
/// ([`crate::DataService::import_scoped`]) is how the granted subset
/// starts syncing here — outside the replica's gossip swarm.
#[derive(Debug, Clone)]
pub struct ScopedPeerGrant {
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
/// carries; importing that namespace stays an explicit data-service act.
/// One grant record exists per granted issuer, carrying its width
/// explicitly — a whole-store ticket, or a capability-scoped grant — so
/// publishing either width replaces the other and a withdrawal is one act.
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

    /// Publish a grant of `issuer`'s data store toward `peer`, into
    /// `identity`'s own metadata store of that connection: a whole-store
    /// ticket, minted here. The issuer must be the granting identity itself
    /// — anything else is refused ([`DelegationUnsupported`]). Replaces any
    /// previous grant for this issuer whatever its width — publishing
    /// whole-store over a scoped grant *is* the widening, one record per
    /// issuer.
    ///
    /// The ticket is a **write** ticket, and deliberately so: the store's
    /// capability is not the access-control mechanism, it is swarm
    /// membership. A whole-store grant is unscoped — the grantee can write
    /// into the namespace, and nothing rejects what it writes.
    async fn publish_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()>;

    /// Read the grants `peer` has published toward hosted `identity`, from
    /// the counterpart's metadata store. Payload-waiting and poll-friendly:
    /// a grant is omitted while its ticket payload has not synced yet — and
    /// likewise when the peer wrote it in a form this version cannot read,
    /// which withholds that grant alone instead of hiding its readable
    /// siblings. An empty list also covers a pair whose directory tickets
    /// have not reached this device yet.
    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>>;

    /// Publish a scoped grant: `identity` grants `peer` read — and, with
    /// `write`, write — on exactly `claims` of `issuer`'s data store. The
    /// issuer must be the granting identity itself — anything else is
    /// refused ([`DelegationUnsupported`]). Capability and ticket travel as
    /// one record (replacing any previous grant of any width); the ticket
    /// carries exactly the granted authority: read-only → a read ticket (no
    /// namespace secret — the grantee cannot write at all), with `write` →
    /// a write ticket (the namespace secret carries write authority — no
    /// ingest hook is installed, ADR-0008).
    async fn publish_scoped_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        issuer: PdnId,
        claims: NonEmpty<ClaimId>,
        write: bool,
    ) -> Result<()>;

    /// Read the scoped grants `peer` has published toward hosted
    /// `identity` — capability and ticket together, with the same
    /// payload-waiting and poll-friendly contract as
    /// [`read_grants`](Self::read_grants).
    async fn read_scoped_grants(
        &self,
        identity: PdnId,
        peer: PdnId,
    ) -> Result<Vec<ScopedPeerGrant>>;
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

    async fn publish_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
        if identity != issuer {
            return Err(DelegationUnsupported { identity, issuer }.into());
        }
        let pair = open_pair(&mut state, identity, peer)
            .await?
            .with_context(|| format!("no connection metadata pair toward {peer}"))?;
        let ticket = state
            .node
            .share_ticket(issuer, ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await?;
        pair.own.publish_grant(issuer, &ticket).await
    }

    async fn read_grants(&self, identity: PdnId, peer: PdnId) -> Result<Vec<PeerGrant>> {
        // Assembly under the lock, polling outside it: the pair's handles
        // are cloned out, so reads do not serialize behind other services.
        let pair = {
            let mut state = self.runtime.state.lock().await;
            state.hosted(identity)?;
            open_pair(&mut state, identity, peer).await?
        };
        let Some(pair) = pair else {
            // No pair reachable yet: not connected, or the directory's
            // tickets have not synced to this device — poll again.
            return Ok(Vec::new());
        };
        let mut grants = Vec::new();
        for issuer in pair.peer.list_grants().await? {
            // Payload-waiting: a grant whose ticket bytes have not arrived —
            // or that the peer wrote unreadably — is omitted, leaving the
            // rest of its grants readable. `?` still surfaces this node's own
            // read failures, which are not the peer's to cause.
            if let Some(ticket) = pair.peer.read_grant(issuer).await? {
                grants.push(PeerGrant { issuer, ticket });
            }
        }
        Ok(grants)
    }

    async fn publish_scoped_grant(
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
        pair.own.publish_scoped_grant(&grant, &ticket).await
    }

    async fn read_scoped_grants(
        &self,
        identity: PdnId,
        peer: PdnId,
    ) -> Result<Vec<ScopedPeerGrant>> {
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
            if let Some((grant, ticket)) = pair.peer.read_scoped_grant(issuer).await? {
                grants.push(ScopedPeerGrant { grant, ticket });
            }
        }
        Ok(grants)
    }
}

/// Keep hosted `identity`'s connections armed for session classification:
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
                let Some(state) = state.upgrade() else { return };
                let mut state = state.lock().await;
                arm_connections(&mut state, identity).await;
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

/// One arming sweep: open every pair `identity`'s directory lists that is
/// not in the cache yet. A pair that cannot open — its tickets still
/// payload-waiting, or a transient store failure — stays cold until the
/// next sweep; a sweep never fails as a whole.
async fn arm_connections(state: &mut State, identity: PdnId) {
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
        if state.metadata_pairs.contains_key(&(identity, peer)) {
            continue;
        }
        let _cold_until_next_sweep = open_pair(state, identity, peer).await;
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
