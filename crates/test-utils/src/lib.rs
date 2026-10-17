//! Shared plumbing for the workspace's scenario tests. A dev-dependency of
//! the crates whose integration tests use it (cargo permits the cycle with
//! `data-layer`); never published.

use std::{
    future::Future,
    time::{Duration, Instant},
};

use anyhow::Result;
use data_layer::{DocTicket, PrivateMetadataStore, SpawnOptions, SyncNode};

pub mod pod;
use pdn_types::{NodeId, PdnId};

/// Counts every panic in this process from here on, on any thread: one on a
/// task the runtime catches, such as the blob store's, fails no test by
/// itself. Nextest runs each test in a process of its own.
pub fn count_panics() -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    let panics = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&panics);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        previous(info);
    }));
    panics
}

/// A node on memory storage — what the in-process suites run on.
pub async fn memory_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions::memory()).await
}

/// The cast: bare [`PdnId`] values, one byte pattern each.
pub mod ids {
    use pdn_types::PdnId;

    pub const ALICE: PdnId = PdnId::from_bytes([0xa1; 32]);
    pub const ALICE_AT_WORK: PdnId = PdnId::from_bytes([0xa2; 32]);
    pub const ALICE_AT_LEISURE: PdnId = PdnId::from_bytes([0xa3; 32]);
    pub const BOB: PdnId = PdnId::from_bytes([0xb0; 32]);
    pub const CAROL: PdnId = PdnId::from_bytes([0xc0; 32]);
    pub const DAVE: PdnId = PdnId::from_bytes([0xd0; 32]);
}

/// Bring up `identity`'s half of `node` the way the product does: its own
/// stores, a PMS naming this device, and the arming that lets a
/// session be judged. A replica of an identity the node does not host is
/// served to nobody, so no scenario reaches a data replica without this.
pub async fn host_identity(node: &SyncNode, identity: PdnId) -> Result<PrivateMetadataStore> {
    node.provision_identity(identity).await?;
    let pms = PrivateMetadataStore::create(node, identity).await?;
    pms.add_device(node.node_id()).await?;
    node.host_identity(identity, &pms)?;
    Ok(pms)
}

/// [`host_identity`] for a further device of an identity whose PMS
/// already exists: import it under that identity and arm.
pub async fn join_identity(
    node: &SyncNode,
    identity: PdnId,
    ticket: DocTicket,
) -> Result<PrivateMetadataStore> {
    node.provision_identity(identity).await?;
    let pms = PrivateMetadataStore::import(node, identity, ticket).await?;
    node.host_identity(identity, &pms)?;
    Ok(pms)
}

/// The liveness budget of a scenario wait: a few of the node's periodic
/// reconcile passes (default 10s), so a convergence rescued by one still
/// fits, and tight enough that a real non-convergence fails in tens of
/// seconds rather than hanging.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Poll `check` every 100ms until it returns `true` or [`TIMEOUT`] elapses.
pub async fn eventually<F, Fut>(mut check: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if check().await? {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the device set of `pms` contains every id in `want`.
pub async fn wait_devices(pms: &PrivateMetadataStore, want: &[NodeId]) -> Result<bool> {
    eventually(|| async {
        let have = pms.list_devices().await?;
        Ok(want.iter().all(|d| have.contains(d)))
    })
    .await
}
