//! Two devices of one identity (Alice) replicate her connections store.
//!
//! Phone and laptop share the same `PdnId`; each gates ingest with
//! `SelfOwned { me: alice }`. Connect/disconnect happen on phone; the
//! laptop sees them replicate through plain pdn-store sync. No second
//! identity and no data-namespace gating — that is a deferred follow-up.

use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{AddrInfoOptions, ConnectionsStore, SelfOwned, ShareMode, SyncNode};
use pdn_types::PdnId;

/// Poll `is_connected(peer)` on `store` until it equals `want`, or time out.
///
/// The 30s ceiling is a liveness bound (must *eventually* replicate), not a
/// correctness one — a larger value only tolerates slow/loaded CI runners.
async fn wait_connected(
    store: &ConnectionsStore,
    peer: PdnId,
    want: bool,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if store.is_connected(peer).await? == want {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_two_devices() -> Result<()> {
    let alice = PdnId::from_bytes([0xa1; 32]);
    let bob = PdnId::from_bytes([0xb0; 32]); // a peer to (dis)connect; no Bob node here

    // Two devices of Alice, each enforcing Invariant 1 at ingest.
    let mut phone = SyncNode::spawn(SelfOwned::new(alice)).await?;
    let mut laptop = SyncNode::spawn(SelfOwned::new(alice)).await?;

    // Phone owns the connections store and already has a connection to Bob
    // before the laptop links — so the laptop must catch this up via the
    // initial set-reconciliation when it imports.
    let phone_conns = ConnectionsStore::create(&mut phone, alice).await?;
    phone_conns.connect(bob).await?;

    let ticket = phone_conns
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_conns = ConnectionsStore::import(&mut laptop, alice, ticket).await?;

    // Catch-up: Bob replicates to laptop (reconciliation on import).
    assert!(
        wait_connected(&laptop_conns, bob, true, Duration::from_secs(30)).await?,
        "laptop did not catch up connect(bob) from phone"
    );

    // Live update: a fresh disconnect on phone propagates to laptop (the
    // swarm is joined by now), and the tombstone flips Bob to not-live.
    phone_conns.disconnect(bob).await?;
    assert!(
        wait_connected(&laptop_conns, bob, false, Duration::from_secs(30)).await?,
        "laptop did not observe disconnect(bob) from phone"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
