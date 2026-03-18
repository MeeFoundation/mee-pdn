use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use super::advertisement::PeerAdvertisement;

/// Cached state for a single discovered peer.
#[derive(Clone, Debug)]
pub struct CachedPeer {
    /// The peer's latest advertisement.
    pub advertisement: PeerAdvertisement,
    /// When we last received/updated this ad (monotonic).
    pub received_at: Instant,
}

/// Summary of a cached peer for external queries.
#[derive(Clone, Debug)]
pub struct CachedPeerInfo {
    /// The peer's identity.
    pub peer_id: [u8; 32],
    // TODO(bloom-filter): Store bloom filter representation instead of raw
    // namespace_ids once advertisement switches to bloom filter.
    /// Namespace IDs from the peer's advertisement.
    pub namespace_ids: Vec<[u8; 32]>,
    /// Advertisement timestamp (unix epoch seconds).
    pub timestamp: u64,
}

/// Cache of discovered peer advertisements.
///
/// Owned exclusively by the event loop task — no Mutex needed.
/// Keyed by `peer_id` ([u8; 32]).
pub struct PeerCache {
    peers: HashMap<[u8; 32], CachedPeer>,
}

impl Default for PeerCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerCache {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// Insert or update a peer's advertisement.
    /// Returns `true` if the ad was new or had a higher version.
    /// Clones the advertisement only when it will be stored.
    pub fn upsert(&mut self, ad: &PeerAdvertisement) -> bool {
        let now = Instant::now();
        if let Some(cached) = self.peers.get(&ad.peer_id) {
            if ad.timestamp <= cached.advertisement.timestamp {
                return false;
            }
        }
        self.peers.insert(
            ad.peer_id,
            CachedPeer {
                advertisement: ad.clone(),
                received_at: now,
            },
        );
        true
    }

    /// Get a peer's cached state.
    pub fn get(&self, peer_id: &[u8; 32]) -> Option<&CachedPeer> {
        self.peers.get(peer_id)
    }

    /// Number of cached peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Return a summary of all cached peers.
    pub fn all_peers(&self) -> Vec<CachedPeerInfo> {
        self.peers
            .values()
            .map(|c| CachedPeerInfo {
                peer_id: c.advertisement.peer_id,
                namespace_ids: c.advertisement.namespace_ids.clone(),
                timestamp: c.advertisement.timestamp,
            })
            .collect()
    }

    /// Remove entries whose `received_at` is older than
    /// `threshold` ago. Returns the evicted peer IDs.
    pub fn evict_stale(&mut self, threshold: Duration) -> Vec<[u8; 32]> {
        let cutoff = Instant::now() - threshold;
        let stale: Vec<[u8; 32]> = self
            .peers
            .iter()
            .filter(|(_, v)| v.received_at < cutoff)
            .map(|(k, _)| *k)
            .collect();
        for id in &stale {
            self.peers.remove(id);
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ad(peer_id: [u8; 32], timestamp: u64) -> PeerAdvertisement {
        let mut ad = PeerAdvertisement {
            peer_id,
            endpoint_ids: vec![[2u8; 32]],
            namespace_ids: vec![[3u8; 32]],
            addresses: vec![],
            timestamp,
            signature: [0u8; 32],
        };
        ad.sign();
        ad
    }

    #[test]
    fn newer_timestamp_replaces() {
        let mut cache = PeerCache::new();
        let ad1 = make_ad([1u8; 32], 1000);
        let ad2 = make_ad([1u8; 32], 2000);
        assert!(cache.upsert(&ad1));
        assert!(cache.upsert(&ad2));
        let cached = cache.get(&[1u8; 32]).expect("exists");
        assert_eq!(cached.advertisement.timestamp, 2000);
    }

    #[test]
    fn older_timestamp_ignored() {
        let mut cache = PeerCache::new();
        let ad2 = make_ad([1u8; 32], 2000);
        let ad1 = make_ad([1u8; 32], 1000);
        assert!(cache.upsert(&ad2));
        assert!(!cache.upsert(&ad1));
        let cached = cache.get(&[1u8; 32]).expect("exists");
        assert_eq!(cached.advertisement.timestamp, 2000);
    }

    #[test]
    fn same_timestamp_ignored() {
        let mut cache = PeerCache::new();
        let ad = make_ad([1u8; 32], 1000);
        assert!(cache.upsert(&ad));
        assert!(!cache.upsert(&ad));
    }

    #[tokio::test]
    async fn staleness_eviction() {
        let mut cache = PeerCache::new();
        let ad = make_ad([1u8; 32], 1);
        cache.upsert(&ad);

        // Immediately evict with zero threshold
        let evicted = cache.evict_stale(Duration::ZERO);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], [1u8; 32]);
        assert!(cache.get(&[1u8; 32]).is_none());
    }

    #[tokio::test]
    async fn staleness_fresh_not_evicted() {
        let mut cache = PeerCache::new();
        let ad = make_ad([1u8; 32], 1);
        cache.upsert(&ad);

        // Large threshold — nothing should be evicted
        let evicted = cache.evict_stale(Duration::from_secs(3600));
        assert!(evicted.is_empty());
        assert!(cache.get(&[1u8; 32]).is_some());
    }
}
