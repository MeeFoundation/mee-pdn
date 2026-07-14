//! Entry listing: metadata enumeration of a data namespace.
//!
//! Single-node scenarios: listing yields exactly the written paths as
//! metadata, the prefix filter matches whole components, and — the paired
//! deny — listing an issuer with no data store on this node fails with
//! `UnknownIssuer`.

use anyhow::Result;
use data_layer::{SyncNode, UnknownIssuer};
use pdn_types::{EntryInfo, EntryPath};
use test_utils::ids;

#[tokio::test(flavor = "multi_thread")]
async fn listing_yields_written_paths_and_prefix_matches_whole_components() -> Result<()> {
    let mut node = SyncNode::spawn().await?;
    let author = node.create_author().await?;
    node.create_namespace(ids::ALICE).await?;

    // Payload lengths 1..=4, so the metadata is checkable per path.
    let paths = ["contacts/a", "contacts/b", "contactsx/c", "profile/name"];
    for (i, path) in paths.iter().enumerate() {
        node.write(
            ids::ALICE,
            author,
            &EntryPath::new(*path)?,
            &vec![7u8; i + 1],
        )
        .await?;
    }

    // Listing yields exactly the written paths, as metadata (no payload
    // bytes to compare — EntryInfo carries none — but lengths line up).
    let mut listed = node.list(ids::ALICE, None).await?;
    listed.sort_by(|a, b| a.path.cmp(&b.path));
    let expected = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            Ok(EntryInfo {
                issuer: ids::ALICE,
                path: EntryPath::new(*path)?,
                payload_len: u64::try_from(i + 1)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(listed, expected);

    // The prefix filter matches whole components: `contacts` matches
    // `contacts/a` and `contacts/b`, not `contactsx/c`.
    let filtered = node
        .list(ids::ALICE, Some(&EntryPath::new("contacts")?))
        .await?;
    let mut filtered_paths: Vec<&str> = filtered.iter().map(|e| e.path.as_str()).collect();
    filtered_paths.sort_unstable();
    assert_eq!(filtered_paths, ["contacts/a", "contacts/b"]);

    // Paired deny: an issuer with no data store on this node is refused as
    // specifically unknown, not a generic failure.
    let err = node.list(ids::BOB, None).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    node.shutdown().await?;
    Ok(())
}
