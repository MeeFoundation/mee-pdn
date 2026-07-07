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

use anyhow::Result;
use data_layer::{
    link_device, provision_identity, AddrInfoOptions, AuthorId, ConnectionsStore, DocTicket,
    IdentityStores, ShareMode, SyncNode, UnknownIssuer,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{ids, wait_connected, wait_devices, wait_entry_is, TIMEOUT};

/// Provision one identity on `node` and lay down its test fixtures: a
/// connection to `peer`, and a data namespace of `issuer` with `value`
/// written at `path`. Returns the phone-side connections handle, the author
/// for further data writes, the linking seed, and the data-namespace ticket
/// (data discovery at linking is deferred — ADR-0009 — so the test hands
/// that ticket over directly).
async fn provision_identity_with_data(
    node: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(ConnectionsStore, AuthorId, DocTicket, DocTicket)> {
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

    Ok((stores.connections, author, seed, data_ticket))
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_identity_two_devices() -> Result<()> {
    // The same path in both data namespaces, a different value in each —
    // mixed-up stores would surface as the wrong value.
    let path = EntryPath::new("k")?;

    // Phone hosts both identities' store sets side by side.
    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let (work_conns, work_author, work_seed, work_data) =
        provision_identity_with_data(&mut phone, ids::ALICE_AT_WORK, ids::BOB, &path, b"work")
            .await?;
    let (_leisure_conns, _leisure_author, leisure_seed, leisure_data) =
        provision_identity_with_data(
            &mut phone,
            ids::ALICE_AT_LEISURE,
            ids::CAROL,
            &path,
            b"leisure",
        )
        .await?;

    // Link the laptop into the work identity only.
    let IdentityStores {
        connections: laptop_work_conns,
        ..
    } = link_device(&mut laptop, work_seed, TIMEOUT).await?;
    assert!(
        wait_connected(&laptop_work_conns, ids::BOB, true).await?,
        "work connections did not replicate to laptop"
    );

    // Isolation: nothing of the leisure identity arrived through that act —
    // its peer is not in the work store, and its namespace is unknown here
    // (specifically unknown, not just any error).
    assert!(!laptop_work_conns.is_connected(ids::CAROL).await?);
    let err = laptop.read(ids::ALICE_AT_LEISURE, &path).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    // Second identity links onto the already-linked node by its own act.
    let IdentityStores {
        private_metadata: laptop_leisure_pms,
        connections: laptop_leisure_conns,
        ..
    } = link_device(&mut laptop, leisure_seed, TIMEOUT).await?;
    assert!(
        wait_connected(&laptop_leisure_conns, ids::CAROL, true).await?,
        "leisure connections did not replicate to laptop"
    );
    assert!(!laptop_leisure_conns.is_connected(ids::BOB).await?);
    assert!(
        wait_devices(&laptop_leisure_pms, &[phone_id, laptop_id]).await?,
        "leisure device set did not converge on laptop"
    );

    // Both identities' data namespaces replicate, each under its own issuer.
    laptop
        .import_namespace(ids::ALICE_AT_WORK, work_data)
        .await?;
    laptop
        .import_namespace(ids::ALICE_AT_LEISURE, leisure_data)
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE_AT_WORK, &path, b"work").await?,
        "work data did not replicate to laptop"
    );
    assert!(
        wait_entry_is(&laptop, ids::ALICE_AT_LEISURE, &path, b"leisure").await?,
        "leisure data did not replicate to laptop"
    );

    // The first identity keeps operating after the second linked: a fresh
    // connection and a fresh data write (an LWW overwrite of the same path)
    // both still reach the laptop.
    work_conns.connect(ids::DAVE).await?;
    assert!(
        wait_connected(&laptop_work_conns, ids::DAVE, true).await?,
        "work connections stopped replicating after the second identity linked"
    );
    phone
        .write(ids::ALICE_AT_WORK, work_author, &path, b"work-2")
        .await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE_AT_WORK, &path, b"work-2").await?,
        "work data stopped replicating after the second identity linked"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
