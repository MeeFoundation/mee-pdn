//! Connections across devices and identities: what one device records, a
//! linked device lists; what one identity records, another never sees.

use anyhow::Result;
use pdn_node::{ConnectionsService as _, IdentityService as _, Runtime};
use test_utils::{eventually, ids, TIMEOUT};

/// Recorded on device A, listed on linked device B — both a connection
/// recorded before linking (catch-up) and one recorded after (live
/// update). Paired isolation: two identities hosted on one runtime keep
/// disjoint connection lists.
#[tokio::test(flavor = "multi_thread")]
async fn recorded_on_one_device_listed_on_the_linked_one_and_disjoint_per_identity() -> Result<()> {
    let a = Runtime::spawn().await?;
    let b = Runtime::spawn().await?;

    // One runtime hosts both identities; each records its own peer.
    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    a.connections().record(x, ids::BOB).await?;
    a.connections().record(y, ids::CAROL).await?;

    // Disjoint on the shared runtime: each identity lists exactly its own
    // peer — the connection of one never shows under the other.
    assert_eq!(a.connections().list(x).await?, vec![ids::BOB]);
    assert_eq!(a.connections().list(y).await?, vec![ids::CAROL]);

    // Link a second device into X: the connection recorded before linking
    // catches up...
    let seed = a.identity().linking_seed(x).await?;
    b.identity().link(x, seed, TIMEOUT).await?;
    assert!(
        eventually(|| async { Ok(b.connections().list(x).await?.contains(&ids::BOB)) }).await?,
        "connection recorded before linking did not reach the linked device"
    );

    // ...and one recorded afterwards replicates live.
    a.connections().record(x, ids::DAVE).await?;
    assert!(
        eventually(|| async { Ok(b.connections().list(x).await?.contains(&ids::DAVE)) }).await?,
        "connection recorded after linking did not reach the linked device"
    );

    // The linked device still sees nothing of Y's connections under X.
    assert!(!b.connections().list(x).await?.contains(&ids::CAROL));

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
