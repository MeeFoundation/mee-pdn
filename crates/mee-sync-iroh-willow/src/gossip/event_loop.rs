//! Background task for gossip receive, broadcast, and eviction.

use std::collections::HashSet;
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
    pub config: GossipConfig,
    pub engine: WillowEngine,
    pub client: iroh_willow::rpc::client::MemClient,
    pub endpoint: Endpoint,
    /// Blob store for reading `_local/` entry payloads.
    pub blobs: iroh_blobs::api::Store,
    /// The node's home namespace, used for `_local/` state.
    pub home_namespace: api::NamespaceId,
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
    refresh_held_namespaces(&mut state).await;
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
                refresh_held_namespaces(&mut state).await;
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
                        refresh_held_namespaces(&mut state).await;
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

    // 5. Namespace intersection (existing held namespaces)
    let matches =
        intersect_namespaces(&ad.namespace_ids, &state.held_namespace_ids);
    if !matches.is_empty()
        && try_auto_connect(state, &ad, &matches).await.is_ok()
    {
        audit_connection(state, &ad.peer_id).await;
        return;
    }

    // 6. Pending invite discovery
    check_pending_invites(state, &ad).await;
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

    crate::add_loopback_addr(&mut addr);

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
        node_hints: vec![own_addr.clone()],
    })
}

async fn broadcast_own_ad(state: &mut EventLoopState) -> Result<(), GossipError> {
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
        timestamp,
        signature: [0u8; 32],
    };
    ad.sign();
    Ok(ad)
}

/// Check if any pending invites match this advertisement.
///
/// A pending invite is a marker at `_local/pending/{subspace}/{namespace}`
/// in the home namespace. When an ad arrives from the matching peer
/// advertising the matching namespace, we auto-connect and delete the
/// marker.
async fn check_pending_invites(state: &mut EventLoopState, ad: &PeerAdvertisement) {
    let Ok(entries) = crate::list_local_entries(
        &state.engine,
        &state.blobs,
        &state.home_namespace,
        "pending/",
    )
    .await
    else {
        return;
    };

    for (suffix, _) in entries {
        // suffix is "{subspace}/{namespace}"
        let Some((sub_hex, ns_hex)) = suffix.split_once('/') else {
            continue;
        };
        let Ok(subspace) = sub_hex.parse::<api::SubspaceId>() else {
            continue;
        };
        let Ok(namespace) = ns_hex.parse::<api::NamespaceId>() else {
            continue;
        };

        // Security: peer_id must match invite's subspace_id
        if ad.peer_id != *subspace.as_bytes() {
            continue;
        }
        // Namespace must be in the advertisement
        if !ad
            .namespace_ids
            .iter()
            .any(|ns| *ns == *namespace.as_bytes())
        {
            continue;
        }

        // Match! Auto-connect using the invite's namespace.
        let ns_bytes = *namespace.as_bytes();
        if try_auto_connect(state, ad, &[ns_bytes]).await.is_ok() {
            audit_connection(state, &ad.peer_id).await;
            // Delete the pending marker
            let del_suffix = format!("pending/{sub_hex}/{ns_hex}");
            let _ = crate::delete_local(
                &state.engine,
                state.owner_user,
                &state.home_namespace,
                &del_suffix,
            )
            .await;
            return;
        }
    }
}

/// If `audit_connections` is enabled, write a marker at
/// `_local/connections/{peer_id_hex}` for observability.
async fn audit_connection(
    state: &EventLoopState,
    peer_id: &[u8; 32],
) {
    if state.config.audit_connections {
        let peer_hex: String =
            peer_id.iter().map(|b| format!("{b:02x}")).collect();
        let suffix = format!("connections/{peer_hex}");
        let _ = crate::write_local_raw(
            &state.engine,
            state.owner_user,
            &state.home_namespace,
            &suffix,
            &[],
        )
        .await;
    }
}

/// Refresh the set of held namespace IDs from both the Willow engine
/// and the `_local/namespaces/imported/` entries in the home namespace.
async fn refresh_held_namespaces(state: &mut EventLoopState) {
    state.held_namespace_ids.clear();
    if let Ok(namespaces) = state.engine.list_namespaces().await {
        for ns in namespaces {
            state.held_namespace_ids.insert(*ns.as_bytes());
        }
    }
    if let Ok(entries) = crate::list_local_entries(
        &state.engine,
        &state.blobs,
        &state.home_namespace,
        "namespaces/imported/",
    )
    .await
    {
        for (suffix, _) in entries {
            if let Ok(id) = suffix.parse::<mee_sync_api::NamespaceId>() {
                state.held_namespace_ids.insert(*id.as_bytes());
            }
        }
    }
}
