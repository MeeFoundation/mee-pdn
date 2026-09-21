//! Entry listing: metadata enumeration of a data namespace, single node.

use anyhow::Result;
use data_layer::UnknownIssuer;
use pdn_types::{EntryInfo, EntryPath};
use test_utils::{host_identity, ids, memory_node};

/// Listing yields exactly the written paths as metadata, and the prefix
/// filter matches whole components. Paired deny: an issuer with no data
/// store on this node is refused as `UnknownIssuer`.
#[tokio::test(flavor = "multi_thread")]
async fn listing_yields_written_paths_and_prefix_matches_whole_components() -> Result<()> {
    let node = memory_node().await?;
    let _directory = host_identity(&node, ids::ALICE).await?;
    let author = node.default_author(ids::ALICE)?;
    node.create_namespace(ids::ALICE, ids::ALICE).await?;

    // Payload lengths 1..=4, so the metadata is checkable per path.
    let paths = [
        "banking/iban",
        "contact/email",
        "contact/phone",
        "contacts/emergency",
    ];
    for (i, path) in paths.iter().enumerate() {
        node.write(
            ids::ALICE,
            ids::ALICE,
            author,
            &EntryPath::new(*path)?,
            &vec![7u8; i + 1],
        )
        .await?;
    }

    // Listing yields exactly the written paths, as metadata (no payload
    // bytes to compare — EntryInfo carries none — but lengths line up).
    let mut listed = node.list(ids::ALICE, ids::ALICE, None).await?;
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

    // The prefix filter matches whole components: `contact` matches
    // `contact/email` and `contact/phone`, not `contacts/emergency`.
    let filtered = node
        .list(ids::ALICE, ids::ALICE, Some(&EntryPath::new("contact")?))
        .await?;
    let mut filtered_paths: Vec<&str> = filtered.iter().map(|e| e.path.as_str()).collect();
    filtered_paths.sort_unstable();
    assert_eq!(filtered_paths, ["contact/email", "contact/phone"]);

    // Paired deny: an issuer with no data store on this node is refused as
    // specifically unknown, not a generic failure.
    let err = node.list(ids::ALICE, ids::BOB, None).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    node.shutdown().await?;
    Ok(())
}
