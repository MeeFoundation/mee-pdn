//! Two devices of one identity (Alice) replicate her stores: the directory
//! and the data namespace she issues. Both nodes host Alice the way the
//! product does — her own stores, her directory naming the device — so a
//! session is judged by her records rather than by ticket possession.

use std::time::Duration;

use anyhow::Result;
use data_layer::{AddrInfoOptions, CatchUpTimeout, PrivateMetadataStore, ShareMode};
use pdn_types::EntryPath;
use test_utils::{
    eventually, host_identity, ids, join_identity, memory_node, wait_connected, wait_entry_is,
    TIMEOUT,
};

/// Both stores replicate to the laptop: the connection and the data entry
/// written before the import arrive as catch-up, a disconnect and a data
/// write made after the swarm is joined arrive live.
///
/// Denied: the laptop is served only once Alice's directory lists it, so
/// the assertion rests on her device set and not on the ticket it holds.
#[tokio::test(flavor = "multi_thread")]
async fn sync_two_devices() -> Result<()> {
    // Two devices of Alice
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    // Phone owns the directory and already has a connection to Bob recorded
    // before the laptop imports — so the laptop must catch this up via the
    // initial set-reconciliation when it imports.
    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    phone_dir.connect(ids::BOB).await?;

    // Phone also issues Alice's data namespace, with one entry written
    // before the laptop imports — same catch-up path, data-namespace store.
    let author = phone.default_author(ids::ALICE)?;
    phone.create_namespace(ids::ALICE, ids::ALICE).await?;
    let name = EntryPath::new("contact/name")?;
    phone
        .write(ids::ALICE, ids::ALICE, author, &name, b"Alice")
        .await?;

    // The laptop joins Alice, and the phone's directory records it as one
    // of her devices — the product's own order, and what makes the phone
    // serve the laptop at all.
    let dir_ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = join_identity(&laptop, ids::ALICE, dir_ticket).await?;
    phone_dir.add_device(laptop.node_id()).await?;
    laptop_dir.add_device(laptop.node_id()).await?;

    let data_ticket = phone
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    laptop
        .import_namespace(ids::ALICE, ids::ALICE, data_ticket)
        .await?;

    // Catch-up: Bob replicates to laptop (reconciliation on import).
    assert!(
        wait_connected(&laptop_dir, ids::BOB, true).await?,
        "laptop did not catch up connect(bob) from phone"
    );

    // Catch-up: the pre-import data entry replicates to laptop.
    assert!(
        wait_entry_is(&laptop, ids::ALICE, ids::ALICE, &name, b"Alice").await?,
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
        .write(ids::ALICE, ids::ALICE, author, &email, b"alice@example.org")
        .await?;
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE,
            ids::ALICE,
            &email,
            b"alice@example.org"
        )
        .await?,
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
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    phone.create_namespace(ids::ALICE, ids::ALICE).await?;
    let phone_author = phone.default_author(ids::ALICE)?;

    let dir_ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = join_identity(&laptop, ids::ALICE, dir_ticket).await?;
    phone_dir.add_device(laptop.node_id()).await?;
    laptop_dir.add_device(laptop.node_id()).await?;
    let laptop_author = laptop.default_author(ids::ALICE)?;

    let ticket = phone
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    laptop
        .import_namespace(ids::ALICE, ids::ALICE, ticket)
        .await?;

    // Both devices write the contested key with no coordination.
    let contested = EntryPath::new("contact/nickname")?;
    phone
        .write(
            ids::ALICE,
            ids::ALICE,
            phone_author,
            &contested,
            b"from-phone",
        )
        .await?;
    laptop
        .write(
            ids::ALICE,
            ids::ALICE,
            laptop_author,
            &contested,
            b"from-laptop",
        )
        .await?;

    // Fences: once each side sees the other's fence, sync sessions have run
    // in both directions and had the chance to carry the contested key too.
    let phone_fence = EntryPath::new("fence/phone")?;
    let laptop_fence = EntryPath::new("fence/laptop")?;
    phone
        .write(ids::ALICE, ids::ALICE, phone_author, &phone_fence, b"1")
        .await?;
    laptop
        .write(ids::ALICE, ids::ALICE, laptop_author, &laptop_fence, b"1")
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE, ids::ALICE, &phone_fence, b"1").await?,
        "phone's fence did not reach laptop"
    );
    assert!(
        wait_entry_is(&phone, ids::ALICE, ids::ALICE, &laptop_fence, b"1").await?,
        "laptop's fence did not reach phone"
    );

    // Both replicas must now agree on the contested key.
    let converged = eventually(|| async {
        let on_phone = phone.read(ids::ALICE, ids::ALICE, &contested).await?;
        let on_laptop = laptop.read(ids::ALICE, ids::ALICE, &contested).await?;
        Ok(on_phone.is_some() && on_phone == on_laptop)
    })
    .await?;
    assert!(converged, "replicas did not converge on the contested key");
    let value = phone
        .read(ids::ALICE, ids::ALICE, &contested)
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

/// A write at a shorter path leaves the entries at longer paths sharing its
/// components or its bytes standing, on the writing device and on its
/// sibling.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_at_a_shorter_path_leaves_the_longer_ones_standing() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    phone.create_namespace(ids::ALICE, ids::ALICE).await?;
    let author = phone.default_author(ids::ALICE)?;

    let dir_ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = join_identity(&laptop, ids::ALICE, dir_ticket).await?;
    phone_dir.add_device(laptop.node_id()).await?;
    laptop_dir.add_device(laptop.node_id()).await?;
    let ticket = phone
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    laptop
        .import_namespace(ids::ALICE, ids::ALICE, ticket)
        .await?;

    let written: [(&str, &[u8]); 3] = [
        ("contact/email", b"email"),
        ("contacts/emergency", b"emergency"),
        ("contact", b"contact"),
    ];
    for (path, payload) in written {
        phone
            .write(
                ids::ALICE,
                ids::ALICE,
                author,
                &EntryPath::new(path)?,
                payload,
            )
            .await?;
    }

    for node in [&phone, &laptop] {
        for (path, payload) in written {
            assert!(
                wait_entry_is(
                    node,
                    ids::ALICE,
                    ids::ALICE,
                    &EntryPath::new(path)?,
                    payload
                )
                .await?,
                "{path} does not read what was written at it"
            );
        }
        let mut listed: Vec<String> = node
            .list(ids::ALICE, ids::ALICE, None)
            .await?
            .into_iter()
            .map(|entry| entry.path.as_str().to_owned())
            .collect();
        listed.sort_unstable();
        assert_eq!(listed, ["contact", "contact/email", "contacts/emergency"]);
    }

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
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    // Any ticket serves as payload — here, another fresh replica's.
    let payload_ticket = PrivateMetadataStore::create(&phone, ids::ALICE)
        .await?
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    phone_dir.put_ticket("data", &payload_ticket).await?;

    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_dir = join_identity(&laptop, ids::ALICE, ticket).await?;
    phone_dir.add_device(laptop.node_id()).await?;

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
/// on arrived content: the first wait, watched from before the arming,
/// covers the arming's first session (and the content it carried), and a
/// second wait from a fresh instant — after
/// which no new content will ever arrive — still returns, woken by a later
/// session that found nothing new. A content poll cannot see that session;
/// the wait must.
#[tokio::test(flavor = "multi_thread")]
async fn directory_wait_returns_on_a_session_not_on_content() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    phone_dir.connect(ids::BOB).await?;
    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    laptop.provision_identity(ids::ALICE).await?;
    let laptop_dir = PrivateMetadataStore::import(&laptop, ids::ALICE, ticket).await?;
    let catch_up = laptop_dir.watch_catch_up().await?;
    laptop.host_identity(ids::ALICE, &laptop_dir)?;
    catch_up.wait(TIMEOUT).await?;
    // The session that returned the wait carried the pre-import record.
    assert!(
        laptop_dir.is_connected(ids::BOB).await?,
        "a successful catch-up session must have carried the existing records"
    );

    // From a fresh instant nothing new will arrive — the wait returns on
    // the next completed session alone (the node's periodic reconcile pass).
    laptop_dir.watch_catch_up().await?.wait(TIMEOUT).await?;

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A directory whose only peer is gone cannot catch up: the wait fails with
/// the distinguishable timeout, not a hang and not a success.
#[tokio::test(flavor = "multi_thread")]
async fn directory_wait_times_out_without_a_reachable_peer() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_dir = host_identity(&phone, ids::ALICE).await?;
    let ticket = phone_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    // The ticket's only contact goes away before the import.
    phone.shutdown().await?;

    laptop.provision_identity(ids::ALICE).await?;
    let laptop_dir = PrivateMetadataStore::import(&laptop, ids::ALICE, ticket).await?;
    let catch_up = laptop_dir.watch_catch_up().await?;
    laptop.host_identity(ids::ALICE, &laptop_dir)?;
    let err = catch_up.wait(Duration::from_secs(2)).await.unwrap_err();
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
    let node = memory_node().await?;
    let _directory = host_identity(&node, ids::ALICE).await?;
    let author = node.default_author(ids::ALICE)?;
    node.create_namespace(ids::ALICE, ids::ALICE).await?;
    let path = EntryPath::new("contact/email")?;

    // On a fresh path: rejected, nothing stored.
    assert!(node
        .write(ids::ALICE, ids::ALICE, author, &path, b"")
        .await
        .is_err());
    assert_eq!(node.read(ids::ALICE, ids::ALICE, &path).await?, None);

    // Over an existing value: rejected, the previous value survives.
    node.write(ids::ALICE, ids::ALICE, author, &path, b"value")
        .await?;
    assert!(node
        .write(ids::ALICE, ids::ALICE, author, &path, b"")
        .await
        .is_err());
    assert_eq!(
        node.read(ids::ALICE, ids::ALICE, &path).await?.as_deref(),
        Some(b"value".as_ref())
    );

    node.shutdown().await?;
    Ok(())
}
