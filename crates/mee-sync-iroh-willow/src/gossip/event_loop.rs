//! Background task for gossip receive, broadcast, and eviction.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use iroh::Endpoint;
use iroh_gossip::api::{GossipReceiver, GossipSender};
use iroh_willow::Engine as WillowEngine;
use mee_sync_api as api;
use tokio::sync::mpsc;

use super::advertisement::PeerAdvertisement;
use super::config::GossipConfig;
use super::error::GossipError;
use super::matching::intersect_namespaces;
use super::peer_cache::PeerCache;
use super::GossipCommand;
use crate::{from_iroh_addr, send_ticket};

/// State owned by the event loop task.
pub(crate) struct EventLoopState {
    pub sender: GossipSender,
    pub peer_cache: PeerCache,
    pub held_namespace_ids: HashSet<[u8; 32]>,
    pub ad_version: u64,
    pub config: GossipConfig,
    /// Kept for future use (async namespace listing).
    pub _engine: WillowEngine,
    pub client: iroh_willow::rpc::client::MemClient,
    pub endpoint: Endpoint,
    pub imported_namespaces: Arc<Mutex<HashSet<api::NamespaceId>>>,
    pub owner_user: iroh_willow::proto::keys::UserId,
}

/// Run the gossip event loop until shutdown.
#[allow(clippy::expect_used)]
pub(crate) async fn run(
    mut state: EventLoopState,
    mut receiver: GossipReceiver,
    mut cmd_rx: mpsc::Receiver<GossipCommand>,
) {
    // Initial namespace refresh + broadcast
    refresh_held_namespaces(&mut state);
    let _ = broadcast_own_ad(&mut state).await;

    let mut rebroadcast = tokio::time::interval(state.config.rebroadcast_interval);
    let mut eviction = tokio::time::interval(state.config.eviction_threshold);

    loop {
        tokio::select! {
            event = receiver.next() => {
                match event {
                    Some(Ok(ev)) => {
                        handle_event(&mut state, ev).await;
                    }
                    Some(Err(_)) | None => break,
                }
            }

            _ = rebroadcast.tick() => {
                refresh_held_namespaces(&mut state);
                let _ =
                    broadcast_own_ad(&mut state).await;
            }

            _ = eviction.tick() => {
                state.peer_cache.evict_stale(
                    state.config.eviction_threshold,
                );
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(GossipCommand::Broadcast) => {
                        refresh_held_namespaces(&mut state);
                        let _ =
                            broadcast_own_ad(&mut state).await;
                    }
                    Some(GossipCommand::JoinPeers(peers)) => {
                        let _ = state
                            .sender
                            .join_peers(peers)
                            .await;
                    }
                    Some(GossipCommand::QueryPeerCount(tx)) => {
                        let _ =
                            tx.send(state.peer_cache.len());
                    }
                    Some(GossipCommand::QueryPeers(tx)) => {
                        let _ = tx.send(
                            state.peer_cache.all_peers(),
                        );
                    }
                    Some(GossipCommand::Shutdown) | None => {
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_event(state: &mut EventLoopState, event: iroh_gossip::api::Event) {
    match event {
        iroh_gossip::api::Event::Received(msg) => {
            handle_message(state, &msg.content).await;
        }
        iroh_gossip::api::Event::NeighborUp(_)
        | iroh_gossip::api::Event::NeighborDown(_)
        | iroh_gossip::api::Event::Lagged => {}
    }
}

async fn handle_message(state: &mut EventLoopState, content: &[u8]) {
    // 1. Deserialize
    let Ok(ad) = PeerAdvertisement::from_bytes(content) else {
        return;
    };

    // 2. Verify signature
    if !ad.verify_signature() {
        return;
    }

    // 3. Skip own advertisements
    if ad.peer_id == *state.owner_user.as_bytes() {
        return;
    }

    // 4. Version check + cache update
    if !state.peer_cache.upsert(&ad) {
        return;
    }

    // 5. Namespace intersection
    let matches = intersect_namespaces(&ad.namespace_ids, &state.held_namespace_ids);
    if matches.is_empty() {
        return;
    }

    // 6. Skip if already connected
    if let Some(cached) = state.peer_cache.get(&ad.peer_id) {
        if cached.connected {
            return;
        }
    }

    // 7. Auto-connect
    if try_auto_connect(state, &ad, &matches).await.is_ok() {
        state.peer_cache.set_connected(&ad.peer_id, true);
    }
}

async fn try_auto_connect(
    state: &EventLoopState,
    ad: &PeerAdvertisement,
    matched_namespaces: &[[u8; 32]],
) -> Result<(), GossipError> {
    // Use the first endpoint ID from the ad
    let endpoint_id_bytes = ad
        .endpoint_ids
        .first()
        .ok_or_else(|| GossipError::InvalidAdvertisement("no endpoint IDs".to_owned()))?;

    // Build peer address from endpoint ID + advertised addresses
    let node_id = iroh::EndpointId::from_bytes(endpoint_id_bytes)
        .map_err(|e| GossipError::InvalidAdvertisement(e.to_string()))?;
    let transport_addrs: Vec<iroh::TransportAddr> = ad
        .addresses
        .iter()
        .filter_map(|s| s.parse::<std::net::SocketAddr>().ok())
        .map(iroh::TransportAddr::Ip)
        .collect();
    let iroh_addr = iroh::EndpointAddr::from_parts(node_id, transport_addrs.into_iter());
    let peer_addr = from_iroh_addr(&iroh_addr);
    let peer_subspace = api::SubspaceId::from_bytes(ad.peer_id);

    // Fetch our own address once for all namespace tickets
    let own_addr = fetch_own_addr(&state.client).await?;

    // Share our matched namespaces with the discovered peer
    for ns_bytes in matched_namespaces {
        let ns_id = api::NamespaceId::from_bytes(*ns_bytes);
        let ticket_result = share_namespace(&state.client, &ns_id, &peer_subspace, &own_addr).await;

        let Ok(ticket) = ticket_result else {
            continue;
        };

        let _ = send_ticket(&state.endpoint, &peer_addr, ticket).await;
    }

    Ok(())
}

/// Fetch our own node address with loopback added for local connections.
async fn fetch_own_addr(
    client: &iroh_willow::rpc::client::MemClient,
) -> Result<api::NodeAddr, GossipError> {
    let mut addr = client
        .node_addr()
        .await
        .map_err(|e| GossipError::Protocol(e.to_string()))?;

    let first_port = addr.ip_addrs().next().map(std::net::SocketAddr::port);
    if let Some(port) = first_port {
        let loopback: std::net::SocketAddr = format!("127.0.0.1:{port}")
            .parse()
            .map_err(|e: std::net::AddrParseError| GossipError::Protocol(e.to_string()))?;
        addr.addrs.insert(iroh::TransportAddr::Ip(loopback));
    }

    Ok(crate::from_iroh_addr(&addr))
}

/// Delegate capabilities for a namespace and build a ticket.
async fn share_namespace(
    client: &iroh_willow::rpc::client::MemClient,
    ns: &api::NamespaceId,
    to: &api::SubspaceId,
    own_addr: &api::NodeAddr,
) -> Result<api::SyncTicket, GossipError> {
    use iroh_willow::interest::{CapSelector, DelegateTo, RestrictArea};
    use iroh_willow::proto::meadowcap::AccessMode;

    let willow_ns = crate::to_willow_ns(ns);
    let willow_user = crate::to_willow_user(to);

    let caps = client
        .delegate_caps(
            CapSelector::any(willow_ns),
            AccessMode::Read,
            DelegateTo::new(willow_user, RestrictArea::None),
        )
        .await
        .map_err(|e| GossipError::Protocol(e.to_string()))?;

    Ok(api::SyncTicket {
        caps: caps
            .into_iter()
            .filter_map(|c| serde_json::to_value(&c).ok())
            .collect(),
        nodes: vec![own_addr.clone()],
    })
}

async fn broadcast_own_ad(state: &mut EventLoopState) -> Result<(), GossipError> {
    state.ad_version += 1;
    let ad = build_own_ad(state).await?;
    let data = ad.to_bytes()?;
    state
        .sender
        .broadcast(data)
        .await
        .map_err(|e| GossipError::Protocol(e.to_string()))
}

async fn build_own_ad(state: &EventLoopState) -> Result<PeerAdvertisement, GossipError> {
    let peer_id = *state.owner_user.as_bytes();
    let endpoint_id = *state.endpoint.id().as_bytes();
    let namespace_ids: Vec<[u8; 32]> = state.held_namespace_ids.iter().copied().collect();

    let addr = state
        .client
        .node_addr()
        .await
        .map_err(|e| GossipError::Protocol(e.to_string()))?;
    let addresses: Vec<String> = addr
        .ip_addrs()
        .map(std::string::ToString::to_string)
        .collect();

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        // TODO: Log a warning when system clock is before UNIX epoch instead
        // of silently using timestamp 0.
        .unwrap_or(0);

    let mut ad = PeerAdvertisement {
        peer_id,
        endpoint_ids: vec![endpoint_id],
        namespace_ids,
        addresses,
        version: state.ad_version,
        timestamp,
        signature: [0u8; 32],
    };
    ad.sign();
    Ok(ad)
}

// TODO: Use Engine.list_namespaces() (async) instead of only the
// imported_namespaces set. Currently misses locally-created namespaces.
#[allow(clippy::expect_used)]
fn refresh_held_namespaces(state: &mut EventLoopState) {
    // Sync call — reads from the imported_namespaces set
    // which is maintained by do_import_and_sync.
    // Engine.list_namespaces() is async, but we can't
    // easily call it here. For the prototype, we use only
    // the imported_namespaces set which is synchronous.
    let imported = state
        .imported_namespaces
        .lock()
        .expect("imported_namespaces lock poisoned");
    state.held_namespace_ids.clear();
    for ns in imported.iter() {
        state.held_namespace_ids.insert(*ns.as_bytes());
    }
}
