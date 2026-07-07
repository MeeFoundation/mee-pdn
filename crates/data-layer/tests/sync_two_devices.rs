//! Two devices of one identity (Alice) replicate her stores: the
//! connections store and the data namespace she issues.
//!
//! Access is bounded by ticket possession alone — the laptop holds the
//! tickets, so it replicates everything; no ingest filter runs. All writes
//! happen on phone; the laptop sees them replicate through plain pdn-store
//! sync — both as catch-up (writes that precede the import) and live (writes
//! after the swarm is joined). Tickets are handed over directly by the test;
//! discovery through the private metadata directory is the `device_linking`
//! test; several identities on one node is the `multi_identity` test.

use anyhow::Result;
use data_layer::{AddrInfoOptions, ConnectionsStore, ShareMode, SyncNode};
use pdn_types::EntryPath;
use test_utils::{eventually, ids, wait_connected, wait_entry_is};

#[tokio::test(flavor = "multi_thread")]
async fn sync_two_devices() -> Result<()> {
    // Two devices of Alice
    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;

    // Phone owns the connections store and already has a connection to Bob
    // before the laptop links — so the laptop must catch this up via the
    // initial set-reconciliation when it imports.
    let phone_conns = ConnectionsStore::create(&mut phone).await?;
    phone_conns.connect(ids::BOB).await?;

    // Phone also issues Alice's data namespace, with one entry written
    // before the laptop imports — same catch-up path, data-namespace store.
    let author = phone.create_author().await?;
    phone.create_namespace(ids::ALICE).await?;
    let name = EntryPath::new("profile/name")?;
    phone.write(ids::ALICE, author, &name, b"Alice").await?;

    let conns_ticket = phone_conns
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_conns = ConnectionsStore::import(&mut laptop, conns_ticket).await?;

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
        wait_connected(&laptop_conns, ids::BOB, true).await?,
        "laptop did not catch up connect(bob) from phone"
    );

    // Catch-up: the pre-import data entry replicates to laptop.
    assert!(
        wait_entry_is(&laptop, ids::ALICE, &name, b"Alice").await?,
        "laptop did not catch up the profile/name entry from phone"
    );

    // Live update: a fresh disconnect on phone propagates to laptop (the
    // swarm is joined by now), and the tombstone flips Bob to not-live.
    phone_conns.disconnect(ids::BOB).await?;
    assert!(
        wait_connected(&laptop_conns, ids::BOB, false).await?,
        "laptop did not observe disconnect(bob) from phone"
    );

    // Live update: a fresh data write on phone reaches laptop the same way.
    let email = EntryPath::new("profile/email")?;
    phone
        .write(ids::ALICE, author, &email, b"alice@example.org")
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE, &email, b"alice@example.org").await?,
        "laptop did not observe the live profile/email write from phone"
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
    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;

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
    let contested = EntryPath::new("k")?;
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
