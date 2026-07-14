//! In-process embedding: the core embeds as a library, with every service
//! driven directly and no host process or HTTP surface involved.

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, SyncService as _,
};
use pdn_types::EntryPath;
use test_utils::ids;

#[tokio::test(flavor = "multi_thread")]
async fn embeds_as_a_library_and_drives_every_service() -> Result<()> {
    let runtime = Runtime::spawn().await?;

    // Identity: create one on its first device.
    let alice = runtime.identity().create().await?;

    // Sync: the runtime reports its node id and the hosted identity.
    assert_eq!(runtime.sync().node_id(), runtime.node_id());
    assert_eq!(runtime.sync().hosted_identities().await?, vec![alice]);

    // Connections: record and list.
    runtime.connections().record(alice, ids::BOB).await?;
    assert_eq!(runtime.connections().list(alice).await?, vec![ids::BOB]);

    // Data: write, read back, list as metadata.
    let path = EntryPath::new("profile/name")?;
    runtime.data().write(alice, &path, b"alice").await?;
    assert_eq!(
        runtime.data().read(alice, &path).await?.as_deref(),
        Some(&b"alice"[..])
    );
    let listed = runtime.data().list(alice, None).await?;
    assert_eq!(
        listed.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["profile/name"]
    );

    runtime.shutdown().await?;
    Ok(())
}
