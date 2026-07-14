//! Identity linking across runtimes: create on one, link on another by the
//! seed, and the isolation that linking must preserve.

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, SyncService as _,
    UnknownIdentity, UnknownIssuer,
};
use pdn_types::EntryPath;
use test_utils::{eventually, ids, TIMEOUT};

/// Create on runtime A, link runtime B by the seed: B hosts the identity
/// and its connections store converges. Paired deny: linking identity X
/// imports nothing of identity Y that also lives on A, and operations
/// addressed to Y on B are refused as unknown.
#[tokio::test(flavor = "multi_thread")]
async fn create_on_a_link_on_b_converges_and_stays_isolated() -> Result<()> {
    let a = Runtime::spawn().await?;
    let b = Runtime::spawn().await?;

    // A hosts two identities; a connection is recorded under each before
    // any linking, so what B receives is attributable.
    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    a.connections().record(x, ids::BOB).await?;
    a.connections().record(y, ids::CAROL).await?;

    // Link B into X only.
    let seed = a.identity().linking_seed(x).await?;
    b.identity().link(x, seed, TIMEOUT).await?;

    // B hosts X — and only X.
    assert_eq!(b.sync().hosted_identities().await?, vec![x]);

    // X's connections store converges on B.
    assert!(
        eventually(|| async { Ok(b.connections().list(x).await?.contains(&ids::BOB)) }).await?,
        "X's connections did not converge on the linked runtime"
    );

    // Paired deny: nothing of Y arrived through that act. Y is unknown to
    // B's identity-addressed services and to its data namespaces —
    // specifically unknown, not a generic failure.
    let err = b.connections().list(y).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b.connections().record(y, ids::DAVE).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b.data().read(y, &EntryPath::new("k")?).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Hosted identities follow create and link: none on a fresh runtime,
/// exactly the created + linked ones afterwards, node id stable throughout.
#[tokio::test(flavor = "multi_thread")]
async fn hosted_identities_follow_create_and_link() -> Result<()> {
    let a = Runtime::spawn().await?;
    let b = Runtime::spawn().await?;

    // Fresh runtime: no identities, a node id already.
    let node_id = b.sync().node_id();
    assert_eq!(b.sync().hosted_identities().await?, vec![]);

    // One created locally, one linked from A: exactly those two.
    let created = b.identity().create().await?;
    let linked = a.identity().create().await?;
    let seed = a.identity().linking_seed(linked).await?;
    b.identity().link(linked, seed, TIMEOUT).await?;

    let mut hosted = b.sync().hosted_identities().await?;
    hosted.sort_by_key(|identity| *identity.as_bytes());
    let mut expected = vec![created, linked];
    expected.sort_by_key(|identity| *identity.as_bytes());
    assert_eq!(hosted, expected);

    // The node id never moved.
    assert_eq!(b.sync().node_id(), node_id);

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
