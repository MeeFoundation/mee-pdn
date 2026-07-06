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

use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{AddrInfoOptions, ConnectionsStore, ShareMode, SyncNode};
use pdn_types::{EntryPath, PdnId};

/// Poll `is_connected(peer)` on `store` until it equals `want`, or time out.
///
/// The 30s ceiling is a liveness bound (must *eventually* replicate), not a
/// correctness one — a larger value only tolerates slow/loaded CI runners.
async fn wait_connected(
    store: &ConnectionsStore,
    peer: PdnId,
    want: bool,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if store.is_connected(peer).await? == want {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll until `path` is present on `node`, or return `None` once `timeout`
/// elapses.
async fn wait_for_entry(
    node: &SyncNode,
    issuer: PdnId,
    path: &EntryPath,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = node.read(issuer, path).await? {
            return Ok(Some(value));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_two_devices() -> Result<()> {
    let alice = PdnId::from_bytes([0xa1; 32]);
    let bob = PdnId::from_bytes([0xb0; 32]); // a peer to (dis)connect; no Bob node here

    // Two devices of Alice
    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;

    // Phone owns the connections store and already has a connection to Bob
    // before the laptop links — so the laptop must catch this up via the
    // initial set-reconciliation when it imports.
    let phone_conns = ConnectionsStore::create(&mut phone).await?;
    phone_conns.connect(bob).await?;

    // Phone also issues Alice's data namespace, with one entry written
    // before the laptop imports — same catch-up path, data-namespace store.
    let author = phone.create_author().await?;
    phone.create_namespace(alice).await?;
    let name = EntryPath::new("profile/name")?;
    phone.write(alice, author, &name, b"Alice").await?;

    let conns_ticket = phone_conns
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_conns = ConnectionsStore::import(&mut laptop, conns_ticket).await?;

    let data_ticket = phone
        .share_ticket(alice, ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    laptop.import_namespace(alice, data_ticket).await?;

    // Catch-up: Bob replicates to laptop (reconciliation on import).
    assert!(
        wait_connected(&laptop_conns, bob, true, Duration::from_secs(30)).await?,
        "laptop did not catch up connect(bob) from phone"
    );

    // Catch-up: the pre-import data entry replicates to laptop.
    let got = wait_for_entry(&laptop, alice, &name, Duration::from_secs(30)).await?;
    assert_eq!(
        got.as_deref(),
        Some(b"Alice".as_ref()),
        "laptop did not catch up the profile/name entry from phone"
    );

    // Live update: a fresh disconnect on phone propagates to laptop (the
    // swarm is joined by now), and the tombstone flips Bob to not-live.
    phone_conns.disconnect(bob).await?;
    assert!(
        wait_connected(&laptop_conns, bob, false, Duration::from_secs(30)).await?,
        "laptop did not observe disconnect(bob) from phone"
    );

    // Live update: a fresh data write on phone reaches laptop the same way.
    let email = EntryPath::new("profile/email")?;
    phone
        .write(alice, author, &email, b"alice@example.org")
        .await?;
    let got = wait_for_entry(&laptop, alice, &email, Duration::from_secs(30)).await?;
    assert_eq!(
        got.as_deref(),
        Some(b"alice@example.org".as_ref()),
        "laptop did not observe the live profile/email write from phone"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
