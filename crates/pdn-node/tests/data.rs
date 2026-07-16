//! The data service end to end: local write/read/list, the interim
//! whole-store ticket handover between runtimes, and the unknown-issuer
//! denies paired with each allowed path.

use anyhow::Result;
use pdn_node::{DataService as _, IdentityService as _, Runtime, ShareMode, UnknownIssuer};
use pdn_types::EntryPath;
use test_utils::eventually;

#[tokio::test(flavor = "multi_thread")]
async fn writes_read_back_list_exactly_and_hand_over_by_ticket() -> Result<()> {
    let a = Runtime::spawn().await?;
    let b = Runtime::spawn().await?;

    let alice = a.identity().create().await?;
    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;

    // Local write then read.
    a.data().write(alice, &email, b"alice@example.org").await?;
    a.data().write(alice, &phone, b"+1-555-0100").await?;
    assert_eq!(
        a.data().read(alice, &email).await?.as_deref(),
        Some(&b"alice@example.org"[..])
    );

    // Listing yields exactly the written paths, without payload bytes.
    let mut listed: Vec<String> = a
        .data()
        .list(alice, None)
        .await?
        .iter()
        .map(|e| e.path.to_string())
        .collect();
    listed.sort();
    assert_eq!(listed, ["contact/email", "contact/phone"]);

    // Paired deny, before any handover: on B the issuer was neither
    // created nor imported, so read, write, and list are each refused as
    // specifically unknown, and nothing is read, written, or listed.
    let read_err = b.data().read(alice, &email).await.unwrap_err();
    assert!(read_err.downcast_ref::<UnknownIssuer>().is_some());
    let write_err = b
        .data()
        .write(alice, &email, b"intruder")
        .await
        .unwrap_err();
    assert!(write_err.downcast_ref::<UnknownIssuer>().is_some());
    let list_err = b.data().list(alice, None).await.unwrap_err();
    assert!(list_err.downcast_ref::<UnknownIssuer>().is_some());

    // Whole-store handover: A shares the namespace as a ticket, B imports
    // it, and the entries sync over.
    let ticket = a.data().share(alice, ShareMode::Write).await?;
    b.data().import(alice, ticket).await?;
    assert!(
        eventually(|| async {
            Ok(b.data().read(alice, &email).await?.as_deref() == Some(&b"alice@example.org"[..]))
        })
        .await?,
        "shared entries did not sync to the importing runtime"
    );
    assert!(
        eventually(|| async { Ok(b.data().list(alice, None).await?.len() == 2) }).await?,
        "imported namespace did not list the synced entries"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
