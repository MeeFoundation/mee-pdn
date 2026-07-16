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

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    link_device, provision_identity, AddrInfoOptions, ConnectionsStore, IdentityStores,
    PrivateMetadataStore, ShareMode, SyncNode,
};
use test_utils::{eventually, ids, wait_connected, wait_devices, TIMEOUT};

#[tokio::test(flavor = "multi_thread")]
async fn device_linking_bootstrap() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();

    // Existing device: provision Alice's store set, then connect Bob.
    let IdentityStores {
        private_metadata: phone_pms,
        connections: phone_conns,
        ..
    } = provision_identity(&phone).await?;
    phone_conns.connect(ids::BOB).await?;

    // The one thing handed to the new device — the QR payload.
    let seed = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    // New device: bring everything up from that single seed.
    let IdentityStores {
        private_metadata: laptop_pms,
        connections: laptop_conns,
        ..
    } = link_device(&laptop, seed, TIMEOUT).await?;

    // The store discovered through the directory replicates the Bob connection.
    assert!(
        wait_connected(&laptop_conns, ids::BOB, true).await?,
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

/// A directory that never receives the connections ticket fails linking with
/// a timeout error — the documented behavior — rather than hanging.
#[tokio::test(flavor = "multi_thread")]
async fn linking_times_out_without_connections_ticket() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    // A bare directory: no connections ticket is ever published into it.
    let pms = PrivateMetadataStore::create(&phone).await?;
    let seed = pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let err = link_device(&laptop, seed, Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("connections ticket did not sync"),
        "expected the timeout error, got: {err:#}"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// Linking twice with the same seed is harmless — the user scanned the QR
/// twice: the stores come up again and the device set holds each device once.
#[tokio::test(flavor = "multi_thread")]
async fn relinking_with_same_seed_is_harmless() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();

    let stores = provision_identity(&phone).await?;
    stores.connections.connect(ids::BOB).await?;
    let seed = stores
        .private_metadata
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let _first = link_device(&laptop, seed.clone(), TIMEOUT).await?;
    let again = link_device(&laptop, seed, TIMEOUT).await?;

    assert!(
        wait_connected(&again.connections, ids::BOB, true).await?,
        "relinked stores did not replicate"
    );
    assert!(
        wait_devices(&again.private_metadata, &[phone_id, laptop_id]).await?,
        "device set did not converge after relinking"
    );
    assert_eq!(
        again.private_metadata.list_devices().await?.len(),
        2,
        "relinking duplicated a device record"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A read-only seed is not enough to link: the newcomer cannot register
/// itself in the device set, and `link_device` reports the failure.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_seed_cannot_link() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    let stores = provision_identity(&phone).await?;
    let read_seed = stores
        .private_metadata
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;

    // The directory replicates (read is enough to sync) and the connections
    // ticket is found, but registering the device is a write — it must fail.
    // The exact error type belongs to the fork, so only failure is asserted.
    let result = link_device(&laptop, read_seed, TIMEOUT).await;
    assert!(result.is_err(), "linking with a read-only seed succeeded");

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// The directory carries tickets of any kind: published on one device, a
/// ticket becomes readable on another once its payload arrives (`get_ticket`
/// is `None` on the record alone).
#[tokio::test(flavor = "multi_thread")]
async fn directory_carries_arbitrary_tickets() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    // Any ticket serves as payload — here, a fresh connections store's.
    let phone_pms = PrivateMetadataStore::create(&phone).await?;
    let payload_ticket = ConnectionsStore::create(&phone)
        .await?
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    phone_pms.put_ticket("data", &payload_ticket).await?;

    let seed = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = PrivateMetadataStore::import(&laptop, seed).await?;

    let arrived =
        eventually(|| async { Ok(laptop_pms.get_ticket("data").await?.is_some()) }).await?;
    assert!(arrived, "the published ticket did not become readable");
    let got = laptop_pms.get_ticket("data").await?.expect("just observed");
    assert_eq!(
        got.to_string(),
        payload_ticket.to_string(),
        "the ticket round-tripped with a different value"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
