//! Test infrastructure for multi-node in-process integration tests.
//!
//! Provides [`TestNode`] (a test-friendly wrapper around [`IrohWillowSyncCore`]),
//! connection helpers, and polling utilities for waiting on sync results.
//!
//! Gated behind `#[cfg(any(test, feature = "test-utils"))]`.

use std::time::Duration;

use futures_util::StreamExt;
use mee_sync_api::{self as api, SyncEngine};

use crate::{DiscoveryConfig, IrohWillowSyncCore};

use crate::gossip;

/// A test-friendly wrapper around the sync core.
///
/// Uses [`DiscoveryConfig::test()`] to enforce localhost-only binding
/// and cleared IP transports for stable multi-homed test behavior.
pub struct TestNode {
    pub core: IrohWillowSyncCore,
    pub label: String,
}

impl TestNode {
    /// Spawn a single test node bound to localhost on an OS-assigned port.
    pub async fn spawn(label: &str) -> Result<Self, api::SyncError> {
        let store = mee_types::LocalStore::new();
        let core = IrohWillowSyncCore::spawn(DiscoveryConfig::test(), store).await?;
        Ok(Self {
            core,
            label: label.to_owned(),
        })
    }

    /// Spawn a test node with gossip discovery enabled.
    pub async fn spawn_with_gossip(label: &str) -> Result<Self, api::SyncError> {
        let mut config = DiscoveryConfig::test();
        config.gossip = Some(gossip::GossipConfig::test());
        let store = mee_types::LocalStore::new();
        let core = IrohWillowSyncCore::spawn(config, store).await?;
        Ok(Self {
            core,
            label: label.to_owned(),
        })
    }

    /// Spawn a test node with a custom gossip config.
    pub async fn spawn_with_gossip_config(
        label: &str,
        gossip_config: gossip::GossipConfig,
    ) -> Result<Self, api::SyncError> {
        let mut config = DiscoveryConfig::test();
        config.gossip = Some(gossip_config);
        let store = mee_types::LocalStore::new();
        let core = IrohWillowSyncCore::spawn(config, store).await?;
        Ok(Self {
            core,
            label: label.to_owned(),
        })
    }

    /// Get the node's address for use in connection helpers.
    pub async fn addr(&self) -> Result<api::NodeAddr, api::SyncError> {
        self.core.addr().await
    }

    /// Get the node's subspace ID.
    pub async fn subspace_id(&self) -> Result<api::SubspaceId, api::SyncError> {
        self.core.subspace_id().await
    }
}

/// Spawn N test nodes with labels "node-0", "node-1", etc.
#[allow(clippy::panic)]
pub async fn spawn_nodes(count: usize) -> Vec<TestNode> {
    let mut nodes = Vec::with_capacity(count);
    for i in 0..count {
        let label = format!("node-{i}");
        let node = TestNode::spawn(&label)
            .await
            .unwrap_or_else(|e| panic!("failed to spawn test node {label}: {e}"));
        nodes.push(node);
    }
    nodes
}

/// Connect two nodes via the connect-and-share flow.
///
/// Creates a namespace on `from`, then connects to `to` sharing that
/// namespace with the given access mode. Returns the shared namespace ID.
pub async fn connect_via_invite(
    from: &IrohWillowSyncCore,
    to: &IrohWillowSyncCore,
    access: api::AccessMode,
) -> Result<api::NamespaceId, api::SyncError> {
    let from_sub = from.subspace_id().await?;
    let ns = from.create_namespace(&from_sub).await?;

    let to_sub = to.subspace_id().await?;
    let to_addr = to.addr().await?;

    from.connect_and_share(&ns, &to_sub, &to_addr, access)
        .await?;

    Ok(ns)
}

/// Poll `get_entries()` until an entry with the given path appears.
///
/// Returns the matching entry, or panics with a descriptive message on timeout.
pub async fn wait_for_entry(
    node: &IrohWillowSyncCore,
    ns: &api::NamespaceId,
    expected_path: &str,
    max_wait: Duration,
) -> api::EntryInfo {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if let Ok(mut stream) = node.get_entries(ns).await {
            while let Some(Ok(entry)) = stream.next().await {
                if entry.path.as_str() == expected_path {
                    return entry;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for entry '{expected_path}' after {max_wait:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `get_entries()` until at least `min_count` entries appear.
///
/// Returns the entries, or panics with a descriptive message on timeout.
pub async fn wait_for_entry_count(
    node: &IrohWillowSyncCore,
    ns: &api::NamespaceId,
    min_count: usize,
    max_wait: Duration,
) -> Vec<api::EntryInfo> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if let Ok(mut stream) = node.get_entries(ns).await {
            let mut entries = Vec::new();
            while let Some(Ok(entry)) = stream.next().await {
                entries.push(entry);
            }
            if entries.len() >= min_count {
                return entries;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: expected >= {min_count} entries after {max_wait:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll the gossip manager's peer cache until at least `min_count`
/// peers are cached. Returns the cached peer info.
pub async fn wait_for_gossip_peer_count(
    manager: &gossip::GossipManager,
    min_count: usize,
    max_wait: Duration,
) -> Vec<gossip::CachedPeerInfo> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if let Ok(peers) = manager.cached_peers().await {
            if peers.len() >= min_count {
                return peers;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: expected >= {min_count} gossip peers \
             after {max_wait:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Connect two gossip-enabled nodes by adding each other's
/// endpoint addresses and joining gossip peers.
///
/// Both nodes must have gossip managers. This establishes
/// gossip mesh connectivity between the pair.
pub async fn join_gossip_peers(a: &TestNode, b: &TestNode) {
    use iroh::address_lookup::memory::MemoryLookup;

    let a_id = a.core.endpoint().id();
    let b_id = b.core.endpoint().id();

    // Get iroh-level addresses
    let a_ep_addr = a.core.endpoint().addr();
    let b_ep_addr = b.core.endpoint().addr();

    // Pre-register addresses via MemoryLookup so iroh
    // can resolve endpoint IDs for gossip connections
    let a_lookup = MemoryLookup::new();
    a_lookup.add_endpoint_info(b_ep_addr);
    a.core.endpoint().address_lookup().add(a_lookup);

    let b_lookup = MemoryLookup::new();
    b_lookup.add_endpoint_info(a_ep_addr);
    b.core.endpoint().address_lookup().add(b_lookup);

    // Tell gossip to connect to each other
    let a_mgr = a.core.gossip_manager().expect("gossip manager");
    let b_mgr = b.core.gossip_manager().expect("gossip manager");

    a_mgr.join_peers(vec![b_id]).await.expect("join");
    b_mgr.join_peers(vec![a_id]).await.expect("join");
}
