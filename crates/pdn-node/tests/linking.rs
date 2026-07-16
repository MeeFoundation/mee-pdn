//! Identity linking across runtimes: create on one, link on another by the
//! seed, and the isolation that linking must preserve.

use anyhow::Result;
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, Runtime, SyncService as _,
    UnknownIdentity, UnknownIssuer,
};
use pdn_types::EntryPath;
use test_utils::{eventually, TIMEOUT};

mod common;
use common::establish_patiently;

/// Create on runtime A, link runtime B by the seed: B hosts the identity
/// and its connections store converges. Paired deny: linking identity X
/// imports nothing of identity Y that also lives on A, and operations
/// addressed to Y on B are refused as unknown.
#[tokio::test(flavor = "multi_thread")]
async fn create_on_a_link_on_b_converges_and_stays_isolated() -> Result<()> {
    let a = Runtime::spawn().await?;
    let b = Runtime::spawn().await?;
    let peers = Runtime::spawn().await?;

    // A hosts two identities; each establishes its own connection before
    // any linking, so what B receives is attributable.
    let x = a.identity().create().await?;
    let y = a.identity().create().await?;
    let pb = peers.identity().create().await?;
    let pc = peers.identity().create().await?;
    let invite = a.connections().invite(x, None).await?;
    establish_patiently(&peers, pb, &a, x, invite).await?;
    let invite = a.connections().invite(y, None).await?;
    establish_patiently(&peers, pc, &a, y, invite).await?;

    // Link B into X only.
    let seed = a.identity().linking_seed(x).await?;
    b.identity().link(x, seed, TIMEOUT).await?;

    // B hosts X — and only X.
    assert_eq!(b.sync().hosted_identities().await?, vec![x]);

    // X's connections store converges on B.
    assert!(
        eventually(|| async { Ok(b.connections().list(x).await?.contains(&pb)) }).await?,
        "X's connections did not converge on the linked runtime"
    );

    // Paired deny: nothing of Y arrived through that act. Y is unknown to
    // B's identity-addressed services — listing, inviting, and both grant
    // operations — and to its data namespaces; specifically unknown, not a
    // generic failure.
    let err = b.connections().list(y).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b.connections().invite(y, None).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b.connections().publish_grant(y, pc, y).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b.connections().read_grants(y, pc).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = b
        .data()
        .read(y, &EntryPath::new("contact/email")?)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    a.shutdown().await?;
    b.shutdown().await?;
    peers.shutdown().await?;
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
