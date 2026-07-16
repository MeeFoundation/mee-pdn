//! Connections across devices and identities: what establishment creates
//! for one identity, a linked device lists; and identities hosted side by
//! side keep disjoint connection lists.

use anyhow::Result;
use pdn_node::{ConnectionsService as _, IdentityService as _, Runtime};
use test_utils::{eventually, TIMEOUT};

mod common;
use common::establish_patiently;

/// One runtime hosts two identities; only one of them establishes a
/// connection. The established connection lists under that identity alone
/// (disjointness), and a device linked into it afterwards catches the
/// connection up through the replicated stores.
#[tokio::test(flavor = "multi_thread")]
async fn established_on_one_device_listed_on_the_linked_one_and_disjoint_per_identity() -> Result<()>
{
    let a = Runtime::spawn().await?;
    let peer = Runtime::spawn().await?;
    let device = Runtime::spawn().await?;

    // A hosts both identities; only X establishes (with P, hosted on the
    // peer runtime — the invite payload travels as a value).
    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    let p = peer.identity().create().await?;
    let invite = a.connections().invite(x, None).await?;
    establish_patiently(&peer, p, &a, x, invite).await?;

    // Disjoint on the shared runtime: X lists its peer, Y lists nothing —
    // the connection of one identity never shows under the other.
    assert_eq!(a.connections().list(x).await?, vec![p]);
    assert_eq!(a.connections().list(y).await?, vec![]);

    // A device linked into X after the establishment catches it up...
    let seed = a.identity().linking_seed(x).await?;
    device.identity().link(x, seed, TIMEOUT).await?;
    assert!(
        eventually(|| async { Ok(device.connections().list(x).await?.contains(&p)) }).await?,
        "the established connection did not reach the linked device"
    );

    // ...and still sees nothing of Y — which it does not even host.
    assert!(device.connections().list(y).await.is_err());

    a.shutdown().await?;
    peer.shutdown().await?;
    device.shutdown().await?;
    Ok(())
}
