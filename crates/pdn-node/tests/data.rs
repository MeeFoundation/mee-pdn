//! The data service end to end: local write/read/list, the unknown-issuer
//! denies paired with each allowed path, and the out-of-band ticket
//! handover as a denial — an armed issuer serves fail-closed, so a ticket
//! alone delivers nothing. The sanctioned channel is the connections grant
//! surface (`establishment` and `scoped_grants` suites).

use std::time::Duration;

use anyhow::Result;
use pdn_node::{
    DataService as _, IdentityService as _, Runtime, ShareMode, SpawnOptions, UnknownIdentity,
    UnknownIssuer,
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
    let bob = b.identity().create().await?;
    let email = EntryPath::new("contact/email")?;
    let phone = EntryPath::new("contact/phone")?;

    // Local write then read.
    a.data()
        .write(alice, alice, &email, b"alice@example.org")
        .await?;
    a.data().write(alice, alice, &phone, b"+1-555-0100").await?;
    assert_eq!(
        a.data().read(alice, alice, &email).await?.as_deref(),
        Some(&b"alice@example.org"[..])
    );

    // Listing yields exactly the written paths, without payload bytes.
    let mut listed: Vec<String> = a
        .data()
        .list(alice, alice, None)
        .await?
        .iter()
        .map(|e| e.path.to_string())
        .collect();
    listed.sort();
    assert_eq!(listed, ["contact/email", "contact/phone"]);

    // Paired deny, before any handover: on B the issuer was neither
    // created nor imported, so read, write, and list are each refused as
    // specifically unknown, and nothing is read, written, or listed.
    let read_err = b.data().read(bob, alice, &email).await.unwrap_err();
    assert!(read_err.downcast_ref::<UnknownIssuer>().is_some());
    let write_err = b
        .data()
        .write(bob, alice, &email, b"intruder")
        .await
        .unwrap_err();
    assert!(write_err.downcast_ref::<UnknownIssuer>().is_some());
    let list_err = b.data().list(bob, alice, None).await.unwrap_err();
    assert!(list_err.downcast_ref::<UnknownIssuer>().is_some());

    // Denied: B resolves to no device and no grant in A's book, so A
    // refuses B's sessions as if the replica were not hosted. The import
    // succeeds as a local registration, several intervals pass, and nothing
    // has arrived.
    let ticket = a.data().share(alice, alice, ShareMode::Write).await?;
    b.data().import(bob, alice, ticket).await?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert!(
        b.data().list(bob, alice, None).await?.is_empty(),
        "a bare ticket must not deliver entries from an armed issuer"
    );
    assert!(b.data().read(bob, alice, &email).await?.is_none());

    // The gossip channel stays closed too: a write made after the import,
    // past any window in which a swarm would have formed, must not arrive.
    let after = EntryPath::new("contact/after")?;
    a.data().write(alice, alice, &after, b"post-import").await?;
    tokio::time::sleep(SWARM_WINDOW).await;
    assert!(
        b.data().list(bob, alice, None).await?.is_empty(),
        "a post-import write must not reach a bare-ticket holder over gossip"
    );
    assert!(b.data().read(bob, alice, &after).await?.is_none());

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Every operation acts for the identity it names and for that one
/// alone: an issuer only a co-located identity holds is unknown to its
/// sibling, exactly as an issuer no identity of this node holds is.
///
/// Denied: the sibling's read, write and list are each refused as
/// specifically unknown, beside the same three succeeding for the
/// identity that holds the issuer.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_acts_for_the_identity_it_names() -> Result<()> {
    let runtime = Runtime::spawn(SpawnOptions::memory()).await?;
    let work = runtime.identity().create().await?;
    let leisure = runtime.identity().create().await?;
    let email = EntryPath::new("contact/email")?;

    runtime
        .data()
        .write(work, work, &email, b"alice@work.example")
        .await?;

    // Allowed: the identity that holds the issuer.
    assert_eq!(
        runtime.data().read(work, work, &email).await?.as_deref(),
        Some(&b"alice@work.example"[..])
    );
    assert_eq!(runtime.data().list(work, work, None).await?.len(), 1);

    // Denied: its co-located sibling, which holds no replica of that
    // issuer, is answered as for an issuer nobody here holds.
    for refusal in [
        runtime.data().read(leisure, work, &email).await.err(),
        runtime
            .data()
            .write(leisure, work, &email, b"intruder")
            .await
            .err(),
        runtime.data().list(leisure, work, None).await.err(),
    ] {
        let refusal = refusal.expect("a co-located identity must not reach the issuer");
        assert!(
            refusal.downcast_ref::<UnknownIssuer>().is_some(),
            "the refusal did not read as an unknown issuer: {refusal:#}"
        );
    }

    runtime.shutdown().await?;
    Ok(())
}

/// A ticket is imported for the identity it is named for, and an import
/// naming an identity this node does not host is refused where the grant
/// binder's own import would be — before anything is registered.
///
/// Denied: the unhosted identity's import refuses as unknown, and the
/// hosted identity beside it gains no issuer from the attempt.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_names_the_identity_it_is_held_for() -> Result<()> {
    let issuer_rt = Runtime::spawn(SpawnOptions::memory()).await?;
    let holder_rt = Runtime::spawn(SpawnOptions::memory()).await?;
    let issuer = issuer_rt.identity().create().await?;
    let holder = holder_rt.identity().create().await?;
    let unhosted = issuer_rt.identity().create().await?;

    let path = EntryPath::new("contact/email")?;
    issuer_rt
        .data()
        .write(issuer, issuer, &path, b"issuer@example.org")
        .await?;
    let ticket = issuer_rt
        .data()
        .share(issuer, issuer, ShareMode::Read)
        .await?;

    // Allowed: the identity that holds it registers the issuer.
    holder_rt
        .data()
        .import(holder, issuer, ticket.clone())
        .await?;
    assert!(
        holder_rt.data().list(holder, issuer, None).await.is_ok(),
        "the importing identity must resolve the issuer it named"
    );

    // Denied: an identity this node does not host, refused at the same
    // call the binder makes.
    let refused = holder_rt
        .data()
        .import(unhosted, issuer, ticket)
        .await
        .expect_err("an import naming an identity this node does not host must be refused");
    assert!(
        refused.downcast_ref::<UnknownIdentity>().is_some(),
        "the refusal did not name the unhosted identity: {refused:#}"
    );
    let unknown = holder_rt
        .data()
        .list(unhosted, issuer, None)
        .await
        .expect_err("the refused import must leave nothing registered");
    assert!(unknown.downcast_ref::<UnknownIdentity>().is_some());

    issuer_rt.shutdown().await?;
    holder_rt.shutdown().await?;
    Ok(())
}
