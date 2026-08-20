use std::time::{Duration, SystemTime};

use anyhow::Result;
use data_layer::{AddrInfoOptions, PrivateMetadataStore, ShareMode};
use test_utils::{eventually, memory_node};

#[tokio::test(flavor = "multi_thread")]
async fn expired_pending_devices_are_reclaimed_after_reimport() -> Result<()> {
    let owner = memory_node().await?;
    let directory = PrivateMetadataStore::create(&owner).await?;
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
    let reopened = PrivateMetadataStore::import(&restarted, ticket).await?;
    assert!(
        eventually(|| async {
            reopened.cleanup_pending_devices().await?;
            Ok(reopened.list_pending_devices().await? == vec![recent.node_id()])
        })
        .await?,
        "reimport cleanup did not retain only the unexpired registration"
    );
    assert!(reopened.list_devices().await?.is_empty());

    abandoned_a.shutdown().await?;
    abandoned_b.shutdown().await?;
    recent.shutdown().await?;
    restarted.shutdown().await?;
    owner.shutdown().await?;
    Ok(())
}
