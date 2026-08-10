use std::time::{Duration, SystemTime};

use anyhow::Result;
use data_layer::{AddrInfoOptions, PrivateMetadataStore, ShareMode, SyncNode};
use test_utils::eventually;

#[tokio::test(flavor = "multi_thread")]
async fn expired_pending_devices_are_reclaimed_after_reimport() -> Result<()> {
    let owner = SyncNode::spawn().await?;
    let directory = PrivateMetadataStore::create(&owner).await?;
    let abandoned_a = SyncNode::spawn().await?;
    let abandoned_b = SyncNode::spawn().await?;
    let recent = SyncNode::spawn().await?;
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
    let restarted = SyncNode::spawn().await?;
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
