use std::time::Duration;

use iroh::EndpointId;

/// Configuration for the gossip discovery subsystem.
pub struct GossipConfig {
    /// How often to re-broadcast own advertisement.
    pub rebroadcast_interval: Duration,
    /// Consider a peer potentially offline after this.
    pub staleness_threshold: Duration,
    /// Evict cached advertisements older than this.
    pub eviction_threshold: Duration,
    /// Bootstrap peers to join the discovery topic.
    pub bootstrap_peers: Vec<EndpointId>,
}

impl GossipConfig {
    /// Sensible defaults for production use.
    pub fn default_config() -> Self {
        Self {
            rebroadcast_interval: Duration::from_secs(45),
            staleness_threshold: Duration::from_secs(600),
            eviction_threshold: Duration::from_secs(3600),
            bootstrap_peers: Vec::new(),
        }
    }

    /// Short intervals for fast integration tests.
    pub fn test() -> Self {
        Self {
            rebroadcast_interval: Duration::from_millis(500),
            staleness_threshold: Duration::from_secs(2),
            eviction_threshold: Duration::from_secs(5),
            bootstrap_peers: Vec::new(),
        }
    }

    /// Set bootstrap peers (builder-style).
    #[must_use]
    pub fn with_bootstrap(mut self, peers: Vec<EndpointId>) -> Self {
        self.bootstrap_peers = peers;
        self
    }
}
