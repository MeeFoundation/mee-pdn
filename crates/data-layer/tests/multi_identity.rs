//! One pair of devices hosts two identities side by side.
//!
//! Alice runs two identities from the same devices — Alice-at-work and
//! Alice-at-leisure — each a `PdnId` of its own with its own store set
//! (private metadata directory, connections store, data namespace). The
//! phone provisions both; the laptop is linked into each by a separate,
//! explicit `link_device` call with that identity's seed. Both identities'
//! stores replicate; they stay isolated (a connection of one never shows
//! under the other); linking the first identity brings nothing of the
//! second; and the first identity keeps operating after the second links.

use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{
    link_device, provision_identity, AddrInfoOptions, ConnectionsStore, DocTicket, LinkedStores,
    ShareMode, SyncNode,
};
use pdn_types::{EntryPath, PdnId};

/// Generous liveness ceiling — a "must eventually replicate" bound, not a
/// correctness one (tolerates slow/loaded CI runners).
const TIMEOUT: Duration = Duration::from_secs(30);

async fn wait_connected(conns: &ConnectionsStore, peer: PdnId, want: bool) -> Result<bool> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if conns.is_connected(peer).await? == want {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_entry(
    node: &SyncNode,
    issuer: PdnId,
    path: &EntryPath,
) -> Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + TIMEOUT;
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

/// Provision one identity on `node` and lay down its test fixtures: a
/// connection to `peer`, and a data namespace of `issuer` with `value`
/// written at `path`. Returns the phone-side connections handle, the linking
/// seed, and the data-namespace ticket (data discovery at linking is
/// deferred — ADR-0009 — so the test hands that ticket over directly).
async fn provision_identity_with_data(
    node: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(ConnectionsStore, DocTicket, DocTicket)> {
    let stores = provision_identity(node).await?;
    stores.connections.connect(peer).await?;
    let seed = stores
        .private_metadata
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let author = node.create_author().await?;
    node.create_namespace(issuer).await?;
    node.write(issuer, author, path, value).await?;
    let data_ticket = node
        .share_ticket(issuer, ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    Ok((stores.connections, seed, data_ticket))
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_identity_two_devices() -> Result<()> {
    let alice_at_work = PdnId::from_bytes([0xa1; 32]);
    let alice_at_leisure = PdnId::from_bytes([0xa2; 32]);
    let bob = PdnId::from_bytes([0xb0; 32]); // work's peer
    let carol = PdnId::from_bytes([0xc0; 32]); // leisure's peer
    let dave = PdnId::from_bytes([0xd0; 32]); // work's peer, connected late

    // The same path in both data namespaces, a different value in each —
    // mixed-up stores would surface as the wrong value.
    let path = EntryPath::new("k")?;

    // Phone hosts both identities' store sets side by side.
    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;
    let (work_conns, work_seed, work_data) =
        provision_identity_with_data(&mut phone, alice_at_work, bob, &path, b"work").await?;
    let (_leisure_conns, leisure_seed, leisure_data) =
        provision_identity_with_data(&mut phone, alice_at_leisure, carol, &path, b"leisure")
            .await?;

    // Link the laptop into the work identity only.
    let LinkedStores {
        connections: laptop_work_conns,
        ..
    } = link_device(&mut laptop, work_seed, TIMEOUT).await?;
    assert!(
        wait_connected(&laptop_work_conns, bob, true).await?,
        "work connections did not replicate to laptop"
    );

    // Isolation: nothing of the leisure identity arrived through that act —
    // its peer is not in the work store, and its namespace is unknown here.
    assert!(!laptop_work_conns.is_connected(carol).await?);
    assert!(laptop.read(alice_at_leisure, &path).await.is_err());

    // Second identity links onto the already-linked node by its own act.
    let LinkedStores {
        private_metadata: laptop_leisure_pms,
        connections: laptop_leisure_conns,
        ..
    } = link_device(&mut laptop, leisure_seed, TIMEOUT).await?;
    assert!(
        wait_connected(&laptop_leisure_conns, carol, true).await?,
        "leisure connections did not replicate to laptop"
    );
    assert!(!laptop_leisure_conns.is_connected(bob).await?);
    assert!(
        !laptop_leisure_pms.list_devices().await?.is_empty(),
        "leisure directory did not replicate to laptop"
    );

    // Both identities' data namespaces replicate, each under its own issuer.
    laptop.import_namespace(alice_at_work, work_data).await?;
    laptop
        .import_namespace(alice_at_leisure, leisure_data)
        .await?;
    let got = wait_for_entry(&laptop, alice_at_work, &path).await?;
    assert_eq!(got.as_deref(), Some(b"work".as_ref()));
    let got = wait_for_entry(&laptop, alice_at_leisure, &path).await?;
    assert_eq!(got.as_deref(), Some(b"leisure".as_ref()));

    // The first identity keeps operating after the second linked.
    work_conns.connect(dave).await?;
    assert!(
        wait_connected(&laptop_work_conns, dave, true).await?,
        "work stores stopped replicating after the second identity linked"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
