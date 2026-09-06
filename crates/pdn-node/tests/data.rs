//! The data service end to end: local write/read/list, the unknown-issuer
//! denies paired with each allowed path, and the out-of-band ticket
//! handover as a denial — an armed issuer serves fail-closed, so a ticket
//! alone delivers nothing. The sanctioned channel is the connections grant
//! surface (`establishment` and `scoped_grants` suites).

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    DataService as _, IdentityService as _, Runtime, ShareMode, SpawnOptions, UnknownIssuer,
};
use pdn_types::EntryPath;

/// "Nothing arrived" is probed by waiting out a few of the ticket holder's
/// reconcile intervals.
const RECONCILE: Duration = Duration::from_millis(500);

/// Absolute, not interval-scaled: a swarm takes around ten seconds to form.
const SWARM_WINDOW: Duration = Duration::from_secs(15);

/// A write reads back and lists exactly on its own runtime, each allowed
/// path paired with the unknown-issuer deny; the out-of-band ticket
/// handover then delivers nothing to a runtime without a grant, over
/// reconciliation and over gossip alike.
#[tokio::test(flavor = "multi_thread")]
async fn writes_read_back_list_exactly_and_hand_over_by_ticket() -> Result<()> {
    let options = SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    };
    let a = Runtime::spawn(options.clone()).await?;
    let b = Runtime::spawn(options).await?;

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

    // Denied: B resolves to no device and no grant in A's book, so A
    // refuses B's sessions as if the replica were not hosted. The import
    // succeeds as a local registration, several intervals pass, and nothing
    // has arrived.
    let ticket = a.data().share(alice, ShareMode::Write).await?;
    b.data().import(alice, ticket).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        b.data().list(alice, None).await?.is_empty(),
        "a bare ticket must not deliver entries from an armed issuer"
    );
    assert!(b.data().read(alice, &email).await?.is_none());

    // The gossip channel stays closed too: a write made after the import,
    // past any window in which a swarm would have formed, must not arrive.
    let after = EntryPath::new("contact/after")?;
    a.data().write(alice, &after, b"post-import").await?;
    tokio::time::sleep(SWARM_WINDOW).await;
    assert!(
        b.data().list(alice, None).await?.is_empty(),
        "a post-import write must not reach a bare-ticket holder over gossip"
    );
    assert!(b.data().read(alice, &after).await?.is_none());

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
