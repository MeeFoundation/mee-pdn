//! Two devices of one identity (Alice) replicate her stores: the directory
//! (with its connections records) and the data namespace she issues.
//!
//! Access is bounded by ticket possession alone — the laptop holds the
//! tickets, so it replicates everything; no ingest filter runs. All writes
//! happen on phone; the laptop sees them replicate through plain pdn-store
//! sync — both as catch-up (writes that precede the import) and live (writes
//! after the swarm is joined). Tickets are handed over directly by the test;
//! several identities on one node is the `multi_identity` test.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use data_layer::{AddrInfoOptions, CatchUpTimeout, PrivateMetadataStore, ShareMode, SyncNode};
use pdn_types::EntryPath;
use test_utils::{eventually, ids, wait_connected, wait_entry_is, TIMEOUT};

#[tokio::test(flavor = "multi_thread")]
async fn sync_two_devices() -> Result<()> {
    // Two devices of Alice
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    // Phone owns the directory and already has a connection to Bob recorded
    // before the laptop imports — so the laptop must catch this up via the
    // initial set-reconciliation when it imports.
    let phone_dir = PrivateMetadataStore::create(&phone).await?;
    phone_dir.connect(ids::BOB).await?;

    // Phone also issues Alice's data namespace, with one entry written
    // before the laptop imports — same catch-up path, data-namespace store.
    let author = phone.create_author().await?;
    phone.create_namespace(ids::ALICE).await?;
    let name = EntryPath::new("contact/name")?;
    phone.write(ids::ALICE, author, &name, b"Alice").await?;

    let dir_ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = PrivateMetadataStore::import(&laptop, dir_ticket).await?;

    let data_ticket = phone
        .share_ticket(
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    laptop.import_namespace(ids::ALICE, data_ticket).await?;

    // Catch-up: Bob replicates to laptop (reconciliation on import).
    assert!(
        wait_connected(&laptop_dir, ids::BOB, true).await?,
        "laptop did not catch up connect(bob) from phone"
    );

    // Catch-up: the pre-import data entry replicates to laptop.
    assert!(
        wait_entry_is(&laptop, ids::ALICE, &name, b"Alice").await?,
        "laptop did not catch up the contact/name entry from phone"
    );

    // Live update: a fresh disconnect on phone propagates to laptop (the
    // swarm is joined by now), and the tombstone flips Bob to not-live.
    phone_dir.disconnect(ids::BOB).await?;
    assert!(
        wait_connected(&laptop_dir, ids::BOB, false).await?,
        "laptop did not observe disconnect(bob) from phone"
    );

    // Live update: a fresh data write on phone reaches laptop the same way.
    let email = EntryPath::new("contact/email")?;
    phone
        .write(ids::ALICE, author, &email, b"alice@example.org")
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE, &email, b"alice@example.org").await?,
        "laptop did not observe the live contact/email write from phone"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// Concurrent writes to the same key on both devices converge: both replicas
/// end up holding the same value. Which write wins is decided by timestamps
/// and is deliberately not asserted — only that the devices agree.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_converge() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    let phone_author = phone.create_author().await?;
    let laptop_author = laptop.create_author().await?;
    phone.create_namespace(ids::ALICE).await?;
    let ticket = phone
        .share_ticket(
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    laptop.import_namespace(ids::ALICE, ticket).await?;

    // Both devices write the contested key with no coordination.
    let contested = EntryPath::new("contact/nickname")?;
    phone
        .write(ids::ALICE, phone_author, &contested, b"from-phone")
        .await?;
    laptop
        .write(ids::ALICE, laptop_author, &contested, b"from-laptop")
        .await?;

    // Fences: once each side sees the other's fence, sync sessions have run
    // in both directions and had the chance to carry the contested key too.
    let phone_fence = EntryPath::new("fence/phone")?;
    let laptop_fence = EntryPath::new("fence/laptop")?;
    phone
        .write(ids::ALICE, phone_author, &phone_fence, b"1")
        .await?;
    laptop
        .write(ids::ALICE, laptop_author, &laptop_fence, b"1")
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE, &phone_fence, b"1").await?,
        "phone's fence did not reach laptop"
    );
    assert!(
        wait_entry_is(&phone, ids::ALICE, &laptop_fence, b"1").await?,
        "laptop's fence did not reach phone"
    );

    // Both replicas must now agree on the contested key.
    let converged = eventually(|| async {
        let on_phone = phone.read(ids::ALICE, &contested).await?;
        let on_laptop = laptop.read(ids::ALICE, &contested).await?;
        Ok(on_phone.is_some() && on_phone == on_laptop)
    })
    .await?;
    assert!(converged, "replicas did not converge on the contested key");
    let value = phone
        .read(ids::ALICE, &contested)
        .await?
        .expect("converged");
    assert!(
        value == b"from-phone" || value == b"from-laptop",
        "converged to a value neither device wrote"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// The directory carries tickets of any kind: published on one device, a
/// ticket becomes readable on another once its payload arrives (`get_ticket`
/// is `None` on the record alone). `data` is the kind creation actually
/// publishes — the identity's own data-namespace ticket.
#[tokio::test(flavor = "multi_thread")]
async fn directory_carries_arbitrary_tickets() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    // Any ticket serves as payload — here, another fresh replica's.
    let phone_dir = PrivateMetadataStore::create(&phone).await?;
    let payload_ticket = PrivateMetadataStore::create(&phone)
        .await?
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    phone_dir.put_ticket("data", &payload_ticket).await?;

    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = PrivateMetadataStore::import(&laptop, ticket).await?;

    let arrived =
        eventually(|| async { Ok(laptop_dir.get_ticket("data").await?.is_some()) }).await?;
    assert!(arrived, "the published ticket did not become readable");
    let got = laptop_dir.get_ticket("data").await?.expect("just observed");
    assert_eq!(
        got.to_string(),
        payload_ticket.to_string(),
        "the ticket round-tripped with a different value"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// The directory's catch-up wait returns on a completed sync session, not
/// on arrived content: the first wait covers the import's own session (and
/// the content it carried), and a second wait from a fresh instant — after
/// which no new content will ever arrive — still returns, woken by a later
/// session that found nothing new. A content poll cannot see that session;
/// the wait must.
#[tokio::test(flavor = "multi_thread")]
async fn directory_wait_returns_on_a_session_not_on_content() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    let phone_dir = PrivateMetadataStore::create(&phone).await?;
    phone_dir.connect(ids::BOB).await?;
    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    // Sessions the import starts count: they start after this instant.
    let before_import = SystemTime::now();
    let laptop_dir = PrivateMetadataStore::import(&laptop, ticket).await?;
    laptop_dir.wait_caught_up(before_import, TIMEOUT).await?;
    // The session that returned the wait carried the pre-import record.
    assert!(
        laptop_dir.is_connected(ids::BOB).await?,
        "a successful catch-up session must have carried the existing records"
    );

    // From a fresh instant nothing new will arrive — the wait returns on
    // the next completed session alone (the node's periodic reconcile pass).
    laptop_dir
        .wait_caught_up(SystemTime::now(), TIMEOUT)
        .await?;

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A directory whose only peer is gone cannot catch up: the wait fails with
/// the distinguishable timeout, not a hang and not a success.
#[tokio::test(flavor = "multi_thread")]
async fn directory_wait_times_out_without_a_reachable_peer() -> Result<()> {
    let phone = SyncNode::spawn().await?;
    let laptop = SyncNode::spawn().await?;

    let phone_dir = PrivateMetadataStore::create(&phone).await?;
    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    // The ticket's only contact goes away before the import.
    phone.shutdown().await?;

    let before_import = SystemTime::now();
    let laptop_dir = PrivateMetadataStore::import(&laptop, ticket).await?;
    let err = laptop_dir
        .wait_caught_up(before_import, Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<CatchUpTimeout>().is_some(),
        "expected the typed catch-up timeout, got: {err:#}"
    );

    laptop.shutdown().await?;
    Ok(())
}

/// An empty payload is not a storable value: zero-length entries are the
/// underlying deletion marker, and writing one is rejected — it neither
/// stores an "empty file" nor deletes the previous value.
#[tokio::test(flavor = "multi_thread")]
async fn empty_payload_write_is_rejected() -> Result<()> {
    let node = SyncNode::spawn().await?;
    let author = node.create_author().await?;
    node.create_namespace(ids::ALICE).await?;
    let path = EntryPath::new("contact/email")?;

    // On a fresh path: rejected, nothing stored.
    assert!(node.write(ids::ALICE, author, &path, b"").await.is_err());
    assert_eq!(node.read(ids::ALICE, &path).await?, None);

    // Over an existing value: rejected, the previous value survives.
    node.write(ids::ALICE, author, &path, b"value").await?;
    assert!(node.write(ids::ALICE, author, &path, b"").await.is_err());
    assert_eq!(
        node.read(ids::ALICE, &path).await?.as_deref(),
        Some(b"value".as_ref())
    );

    node.shutdown().await?;
    Ok(())
}
