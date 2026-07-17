//! The connections service: establish a hosted identity's connections,
//! list them, and carry grants over the connections' metadata pairs.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use data_layer::{AddrInfoOptions, ConnectionMetadata, DocTicket, ShareMode};
use pdn_types::PdnId;

use crate::pairing::{
    establish_via_dialogue, InvitePayload, UnsupportedInviteVersion, DEFAULT_INVITE_LIFETIME,
    INVITE_FORMAT_VERSION,
};
use crate::runtime::{Runtime, State};

/// A grant read from a connected peer's metadata store: the peer granted
/// access to the data store `issuer` issues, and the ticket is the interim
/// whole-store payload — importing it (an explicit data-service act) is how
/// the granted namespace starts syncing here.
#[derive(Debug, Clone)]
pub struct PeerGrant {
    /// The issuer of the granted data store.
    pub issuer: PdnId,
    /// The whole-store ticket the grant carries.
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
/// carries; importing that namespace stays an explicit data-service act
/// (the reactive cascade is a later change). The grant payload is the
/// interim whole-store ticket; capability-scoped grants land with the
/// read-capability mechanism.
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

    /// Publish a grant of `issuer`'s data store — hosted on this node —
    /// toward `peer`, into `identity`'s own metadata store of that
    /// connection: the interim whole-store ticket, minted here.
    ///
    /// The ticket is a **write** ticket, and deliberately so: the store's
    /// capability is not the access-control mechanism, it is swarm
    /// membership. Read and write are `UWill`'s to decide, enforced at
    /// ingest and egress; a read ticket here would only pretend to gate
    /// access while the gates that matter are being built. Until they are, a
    /// grant is whole-store and unscoped — the grantee can write into the
    /// namespace, and nothing yet rejects what it writes.
    async fn publish_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()>;

    /// Read the grants `peer` has published toward hosted `identity`, from
    /// the counterpart's metadata store. Payload-waiting and poll-friendly:
    /// a grant is omitted while its ticket payload has not synced yet — and
    /// likewise when the peer wrote it in a form this version cannot read,
    /// which withholds that grant alone instead of hiding its readable
    /// siblings. An empty list also covers a pair whose directory tickets
    /// have not reached this device yet.
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

    async fn publish_grant(&self, identity: PdnId, peer: PdnId, issuer: PdnId) -> Result<()> {
        let mut state = self.runtime.state.lock().await;
        state.hosted(identity)?;
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
    // doc and an author, as a blanket re-import used to).
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
    state.metadata_pairs.insert((identity, peer), pair.clone());
    Ok(Some(pair))
}
