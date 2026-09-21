use std::time::{Duration, SystemTime};

use anyhow::Result;
use data_layer::{AddrInfoOptions, ShareMode};
use test_utils::{eventually, host_identity, ids, join_identity, memory_node};

/// Pending registrations older than the expiry are dropped by the cleanup
/// on a re-imported directory, a recent one is kept, and none of them is a
/// device.
#[tokio::test(flavor = "multi_thread")]
async fn expired_pending_devices_are_reclaimed_after_reimport() -> Result<()> {
    let owner = memory_node().await?;
    let directory = host_identity(&owner, ids::ALICE).await?;
    let abandoned_a = memory_node().await?;
    let abandoned_b = memory_node().await?;
    let recent = memory_node().await?;
    let old = SystemTime::now() - Duration::from_hours(25);
    directory
        .add_pending_device_at_for_test(abandoned_a.node_id(), old)
        .await?;
    directory
        .add_pending_device_at_for_test(abandoned_b.node_id(), old)
        .await?;
    directory.add_pending_device(recent.node_id()).await?;

    let ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let restarted = memory_node().await?;
    let reopened = join_identity(&restarted, ids::ALICE, ticket).await?;
    directory.add_device(restarted.node_id()).await?;
    assert!(
        eventually(|| async {
            reopened.cleanup_pending_devices().await?;
            Ok(reopened.list_pending_devices().await? == vec![recent.node_id()])
        })
        .await?,
        "reimport cleanup did not retain only the unexpired registration"
    );
    // A pending registration is not a device: only the owner and the
    // device that re-imported are in the set.
    let devices = reopened.list_devices().await?;
    for pending in [
        abandoned_a.node_id(),
        abandoned_b.node_id(),
        recent.node_id(),
    ] {
        assert!(
            !devices.contains(&pending),
            "a pending registration entered the device set"
        );
    }

    abandoned_a.shutdown().await?;
    abandoned_b.shutdown().await?;
    recent.shutdown().await?;
    restarted.shutdown().await?;
    owner.shutdown().await?;
    Ok(())
}
