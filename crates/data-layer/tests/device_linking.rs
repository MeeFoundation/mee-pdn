//! Device linking end to end: a new device of Alice comes up from a single
//! seed — the private-metadata-store ticket (the QR) — and bootstraps the
//! rest through that directory, joining her device set.
//!
//! The existing device (phone) provisions Alice's store set
//! (`provision_identity`) and connects Bob. The new device (laptop) is handed
//! only the directory seed and runs `link_device`: it imports the directory,
//! registers itself, discovers and imports the connections store, and the Bob
//! connection replicates to it. The device set is bidirectional — each device
//! ends up seeing both.

use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{
    link_device, provision_identity, AddrInfoOptions, ConnectionsStore, LinkedStores,
    PrivateMetadataStore, ShareMode, SyncNode,
};
use pdn_types::{NodeId, PdnId};

/// Generous liveness ceiling — a "must eventually replicate" bound, not a
/// correctness one (tolerates slow/loaded CI runners).
const TIMEOUT: Duration = Duration::from_secs(30);

async fn wait_connected(conns: &ConnectionsStore, peer: PdnId) -> Result<bool> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if conns.is_connected(peer).await? {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_devices(pms: &PrivateMetadataStore, want: &[NodeId]) -> Result<bool> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let have = pms.list_devices().await?;
        if want.iter().all(|d| have.contains(d)) {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_linking_bootstrap() -> Result<()> {
    let bob = PdnId::from_bytes([0xb0; 32]);

    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();

    // Existing device: provision Alice's store set, then connect Bob.
    let LinkedStores {
        private_metadata: phone_pms,
        connections: phone_conns,
        ..
    } = provision_identity(&mut phone).await?;
    phone_conns.connect(bob).await?;

    // The one thing handed to the new device — the QR payload.
    let seed = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    // New device: bring everything up from that single seed.
    let LinkedStores {
        private_metadata: laptop_pms,
        connections: laptop_conns,
        ..
    } = link_device(&mut laptop, seed, TIMEOUT).await?;

    // The store discovered through the directory replicates the Bob connection.
    assert!(
        wait_connected(&laptop_conns, bob).await?,
        "the Bob connection did not replicate to laptop via the discovered store"
    );

    // The device set is bidirectional: both devices, on both sides.
    assert!(
        wait_devices(&laptop_pms, &[phone_id, laptop_id]).await?,
        "laptop's device set is missing a device"
    );
    assert!(
        wait_devices(&phone_pms, &[phone_id, laptop_id]).await?,
        "phone did not see the newly linked device"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
