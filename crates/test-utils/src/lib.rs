//! Shared plumbing for the workspace's scenario tests: the
//! poll-until-deadline helper, the replication timeout, and the cast of test
//! identities.
//!
//! A dev-dependency of the crates whose integration tests use it (cargo
//! permits the cycle: this crate depends on `data-layer`, whose tests
//! dev-depend on this crate); never published.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{PrivateMetadataStore, SyncNode};
use pdn_types::{EntryPath, NodeId, PdnId};

/// The cast: bare [`PdnId`] values, one byte pattern each. No node runs for
/// any of them unless a test spawns one — the peers (Bob, Carol, Dave) exist
/// only as connections records in directories.
pub mod ids {
    use pdn_types::PdnId;

    pub const ALICE: PdnId = PdnId::from_bytes([0xa1; 32]);
    pub const ALICE_AT_WORK: PdnId = PdnId::from_bytes([0xa2; 32]);
    pub const ALICE_AT_LEISURE: PdnId = PdnId::from_bytes([0xa3; 32]);
    pub const BOB: PdnId = PdnId::from_bytes([0xb0; 32]);
    pub const CAROL: PdnId = PdnId::from_bytes([0xc0; 32]);
    pub const DAVE: PdnId = PdnId::from_bytes([0xd0; 32]);
}

/// Generous liveness ceiling — a "must eventually replicate" bound, not a
/// correctness one. Generosity is free: polls return the moment their
/// condition holds, so a green run never pays this ceiling, and a larger
/// value only tolerates slow environments — it never makes an assertion
/// wrong. What it must absorb is the cold start every fresh process pays on
/// its first dials — sockets, routes, and peers from zero, plus OS-level
/// vetting of a newly built binary (worst observed: per-binary firewall and
/// scan gating on macOS stalling early packets for tens of seconds when
/// several nodes come up at once). Shrinking it makes no green run faster;
/// it only turns slow-but-healthy cold starts into first-run-only flakes.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// Poll `check` every 100ms until it returns `true` or [`TIMEOUT`] elapses;
/// the return says whether the condition was observed in time.
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

/// Wait until `is_connected(peer)` on the directory `pms` equals `want`.
pub async fn wait_connected(pms: &PrivateMetadataStore, peer: PdnId, want: bool) -> Result<bool> {
    eventually(|| async { Ok(pms.is_connected(peer).await? == want) }).await
}

/// Wait until the entry at `path` under `issuer` reads as exactly `expected`.
pub async fn wait_entry_is(
    node: &SyncNode,
    issuer: PdnId,
    path: &EntryPath,
    expected: &[u8],
) -> Result<bool> {
    eventually(|| async { Ok(node.read(issuer, path).await?.as_deref() == Some(expected)) }).await
}

/// Wait until the device set of `pms` contains every id in `want`.
pub async fn wait_devices(pms: &PrivateMetadataStore, want: &[NodeId]) -> Result<bool> {
    eventually(|| async {
        let have = pms.list_devices().await?;
        Ok(want.iter().all(|d| have.contains(d)))
    })
    .await
}
