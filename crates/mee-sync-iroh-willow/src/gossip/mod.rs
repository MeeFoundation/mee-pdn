//! Gossip-based peer discovery for the mee-pdn network.
//!
//! Peers broadcast `PeerAdvertisement` messages over a shared
//! iroh-gossip topic. Receivers intersect advertised namespace
//! IDs against their held capabilities to discover sync targets.

pub mod advertisement;
pub mod config;
pub mod error;
pub mod event_loop;
pub mod matching;
pub mod peer_cache;

pub use advertisement::PeerAdvertisement;
pub use config::GossipConfig;
pub use error::GossipError;
pub use peer_cache::CachedPeerInfo;

use iroh::Endpoint;
use iroh_gossip::{Gossip, TopicId};
use iroh_willow::Engine as WillowEngine;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Well-known discovery topic for all mee-pdn nodes.
pub fn discovery_topic_id() -> TopicId {
    let hash = Sha256::digest(b"mee-pdn/discovery/v1");
    let bytes: [u8; 32] = hash.into();
    TopicId::from_bytes(bytes)
}

/// Commands sent to the gossip event loop.
pub(crate) enum GossipCommand {
    /// Force an immediate re-broadcast.
    Broadcast,
    /// Add peers to the gossip topic.
    JoinPeers(Vec<iroh::EndpointId>),
    /// Query the number of cached peers.
    QueryPeerCount(tokio::sync::oneshot::Sender<usize>),
    /// Query all cached peer info.
    QueryPeers(tokio::sync::oneshot::Sender<Vec<CachedPeerInfo>>),
    /// Shut down the event loop.
    Shutdown,
}

/// Public handle to the gossip discovery subsystem.
pub struct GossipManager {
    cmd_tx: mpsc::Sender<GossipCommand>,
    task: JoinHandle<()>,
}

impl GossipManager {
    /// Start the gossip discovery subsystem.
    ///
    /// Must be called after the Router is spawned so the
    /// endpoint is ready to accept gossip connections.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        gossip: Gossip,
        endpoint: Endpoint,
        engine: WillowEngine,
        client: iroh_willow::rpc::client::MemClient,
        owner_user: iroh_willow::proto::keys::UserId,
        config: GossipConfig,
        store: mee_types::LocalStore,
    ) -> Result<Self, GossipError> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        Self::start_with_channel(
            gossip, endpoint, engine, client, owner_user, config, cmd_tx, cmd_rx, store,
        )
        .await
    }

    /// Start with a pre-created command channel.
    ///
    /// Used when the sender needs to be shared with other subsystems
    /// (e.g. `ConnectHandler`) before the gossip manager is started.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_with_channel(
        gossip: Gossip,
        endpoint: Endpoint,
        engine: WillowEngine,
        client: iroh_willow::rpc::client::MemClient,
        owner_user: iroh_willow::proto::keys::UserId,
        config: GossipConfig,
        cmd_tx: mpsc::Sender<GossipCommand>,
        cmd_rx: mpsc::Receiver<GossipCommand>,
        store: mee_types::LocalStore,
    ) -> Result<Self, GossipError> {
        let topic_id = discovery_topic_id();
        let topic = gossip
            .subscribe(topic_id, config.bootstrap_peers.clone())
            .await
            .map_err(|e| GossipError::Protocol(e.to_string()))?;

        let (sender, receiver) = topic.split();

        let state = event_loop::EventLoopState {
            sender,
            peer_cache: peer_cache::PeerCache::new(),
            held_namespace_ids: std::collections::HashSet::new(),
            ad_version: 0,
            config,
            engine,
            client,
            endpoint,
            store,
            owner_user,
        };

        let task = tokio::spawn(event_loop::run(state, receiver, cmd_rx));

        Ok(Self { cmd_tx, task })
    }

    /// Request immediate re-broadcast of own advertisement.
    pub async fn trigger_broadcast(&self) -> Result<(), GossipError> {
        self.cmd_tx
            .send(GossipCommand::Broadcast)
            .await
            .map_err(|e| GossipError::Protocol(e.to_string()))
    }

    /// Add peers to the gossip topic for mesh connectivity.
    pub async fn join_peers(&self, peers: Vec<iroh::EndpointId>) -> Result<(), GossipError> {
        self.cmd_tx
            .send(GossipCommand::JoinPeers(peers))
            .await
            .map_err(|e| GossipError::Protocol(e.to_string()))
    }

    /// Query the number of cached peer advertisements.
    pub async fn peer_count(&self) -> Result<usize, GossipError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(GossipCommand::QueryPeerCount(tx))
            .await
            .map_err(|e| GossipError::Protocol(e.to_string()))?;
        rx.await.map_err(|e| GossipError::Protocol(e.to_string()))
    }

    /// Query all cached peer information.
    pub async fn cached_peers(&self) -> Result<Vec<CachedPeerInfo>, GossipError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(GossipCommand::QueryPeers(tx))
            .await
            .map_err(|e| GossipError::Protocol(e.to_string()))?;
        rx.await.map_err(|e| GossipError::Protocol(e.to_string()))
    }

    /// Shut down the gossip subsystem gracefully.
    pub async fn shutdown(self) -> Result<(), GossipError> {
        let _ = self.cmd_tx.send(GossipCommand::Shutdown).await;
        let _ = self.task.await;
        Ok(())
    }
}
