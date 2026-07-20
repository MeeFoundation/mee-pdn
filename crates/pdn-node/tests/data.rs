//! The data service end to end: local write/read/list, the unknown-issuer
//! denies paired with each allowed path, and the out-of-band ticket
//! handover — a denial: an armed issuer serves fail-closed, so a ticket
//! alone delivers nothing. The sanctioned channels are the connections
//! grant surface (whole-store: `establishment` suite; scoped:
//! `scoped_grants` suite).

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    DataService as _, IdentityService as _, Runtime, ShareMode, SpawnOptions, UnknownIssuer,
};
use pdn_types::EntryPath;

/// The reconcile cadence this scenario runs at: the ticket holder's only
/// path is classified reconciliation, so "nothing arrived" is probed by
/// waiting out a few of its intervals — milliseconds here instead of the
/// tens of seconds the production default would cost.
const RECONCILE: Duration = Duration::from_millis(500);

/// How long a would-be gossip delivery gets before "it never came" counts.
/// Deliberately absolute, not interval-scaled: swarm formation and
/// broadcast latency are gossip-stack behaviour, independent of the
/// reconcile cadence, and a swarm takes around ten seconds to form.
const SWARM_WINDOW: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn writes_read_back_list_exactly_and_hand_over_by_ticket() -> Result<()> {
    let options = SpawnOptions {
        reconcile_interval: RECONCILE,
    };
    let a = Runtime::spawn_with(options.clone()).await?;
    let b = Runtime::spawn_with(options).await?;

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

    // Denied: an out-of-band ticket does not deliver. A's identity is
    // armed at creation, and B's runtime resolves to no device and no
    // grant in A's book, so A refuses B's sessions as if the replica were
    // not hosted. The import itself succeeds (a local registration),
    // several reconcile intervals pass — and nothing has arrived. Delivery
    // requires a recorded grant over a connection: the whole-store grant
    // flow lives in the `establishment` suite, the scoped one in
    // `scoped_grants`.
    let ticket = a.data().share(alice, ShareMode::Write).await?;
    b.data().import(alice, ticket).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        b.data().list(alice, None).await?.is_empty(),
        "a bare ticket must not deliver entries from an armed issuer"
    );
    assert!(b.data().read(alice, &email).await?.is_none());

    // The gossip channel stays closed too: a grantee import never joins
    // the issuer's swarm, so a write made *after* the import — past any
    // window in which a swarm would have formed — must not arrive either.
    // [`SWARM_WINDOW`] bounds the wait.
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
