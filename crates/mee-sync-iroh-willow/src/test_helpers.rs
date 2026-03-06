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
        let core = IrohWillowSyncCore::spawn(DiscoveryConfig::test()).await?;
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
