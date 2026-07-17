//! Three devices, two identities, a partial overlap — and no founder.
//!
//! Phone brings up Alice-at-work (connected to Bob, one data entry) and
//! Alice-at-leisure (connected to Carol, one data entry). Laptop imports
//! both store sets from phone's tickets. Tablet imports the work identity
//! only — and its tickets come from the **laptop**, not the founder, proving
//! a device that replicated a store is a full peer: what it holds is
//! sufficient to bring up the next device. Device sets end up asymmetric
//! (work: three, leisure: two), live updates cross the three-device swarm,
//! and the tablet knows nothing of the leisure identity.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, DocTicket, PrivateMetadataStore, ShareMode, SyncNode, UnknownIssuer,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{ids, wait_connected, wait_devices, wait_entry_is};

/// Bring one identity up on `phone` with its fixtures: a directory with the
/// phone registered and a connection to `peer`, plus `value` at `path` in
/// the data namespace of `issuer`. Returns the phone-side directory and its
/// write ticket.
async fn provision_with_fixtures(
    phone: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(PrivateMetadataStore, DocTicket)> {
    let directory = PrivateMetadataStore::create(phone).await?;
    directory.add_device(phone.node_id()).await?;
    directory.connect(peer).await?;
    let author = phone.create_author().await?;
    phone.create_namespace(issuer).await?;
    phone.write(issuer, author, path, value).await?;
    let ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    Ok((directory, ticket))
}

/// Bring the identity behind `ticket` up on `node`, as device linking does
/// at the store level: import the directory and join the device set.
async fn join_from(node: &SyncNode, ticket: DocTicket) -> Result<PrivateMetadataStore> {
    let directory = PrivateMetadataStore::import(node, ticket).await?;
    directory.add_device(node.node_id()).await?;
    Ok(directory)
}

/// Hand `issuer`'s data namespace from one node to another by ticket.
async fn import_data_from(from: &SyncNode, to: &mut SyncNode, issuer: PdnId) -> Result<()> {
    let ticket = from
        .share_ticket(issuer, ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    to.import_namespace(issuer, ticket).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn three_devices_two_identities() -> Result<()> {
    let path = EntryPath::new("affiliation/group")?;

    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;
    let mut tablet = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let tablet_id = tablet.node_id();

    // Phone brings up both identities, each with a connection and one entry.
    let (work_phone_dir, work_ticket) = provision_with_fixtures(
        &mut phone,
        ids::ALICE_AT_WORK,
        ids::BOB,
        &path,
        b"Acme Engineering",
    )
    .await?;
    let (_leisure_phone_dir, leisure_ticket) = provision_with_fixtures(
        &mut phone,
        ids::ALICE_AT_LEISURE,
        ids::CAROL,
        &path,
        b"Boston Bridge Club",
    )
    .await?;

    // Laptop joins both identities from phone's tickets and imports the
    // work data namespace.
    let work_laptop_dir = join_from(&laptop, work_ticket).await?;
    let leisure_laptop_dir = join_from(&laptop, leisure_ticket).await?;
    import_data_from(&phone, &mut laptop, ids::ALICE_AT_WORK).await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE_AT_WORK, &path, b"Acme Engineering").await?,
        "work data did not reach laptop"
    );

    // Tablet joins work only — directory and data tickets issued by the
    // LAPTOP.
    let tablet_ticket = work_laptop_dir
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let work_tablet_dir = join_from(&tablet, tablet_ticket).await?;
    import_data_from(&laptop, &mut tablet, ids::ALICE_AT_WORK).await?;

    // Transitive catch-up: state authored on phone reaches the tablet
    // through stores it obtained via the laptop.
    assert!(
        wait_connected(&work_tablet_dir, ids::BOB, true).await?,
        "the Bob connection did not reach the tablet"
    );
    assert!(
        wait_entry_is(&tablet, ids::ALICE_AT_WORK, &path, b"Acme Engineering").await?,
        "work data did not reach the tablet"
    );

    // Live through the three-device swarm: a fresh connection on phone.
    work_phone_dir.connect(ids::DAVE).await?;
    assert!(
        wait_connected(&work_tablet_dir, ids::DAVE, true).await?,
        "a live work update did not reach the tablet"
    );

    // The work device set converges to all three — on the founder too.
    let all = [phone_id, laptop_id, tablet_id];
    assert!(
        wait_devices(&work_tablet_dir, &all).await?,
        "the tablet's work device set is incomplete"
    );
    assert!(
        wait_devices(&work_phone_dir, &all).await?,
        "the phone's work device set is incomplete"
    );

    // The leisure device set stays at two: the tablet is not in it.
    assert!(
        wait_devices(&leisure_laptop_dir, &[phone_id, laptop_id]).await?,
        "the leisure device set did not converge"
    );
    assert!(
        !leisure_laptop_dir
            .list_devices()
            .await?
            .contains(&tablet_id),
        "the tablet leaked into the leisure device set"
    );

    // And the tablet knows nothing of leisure: the namespace is unknown there.
    let err = tablet.read(ids::ALICE_AT_LEISURE, &path).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    phone.shutdown().await?;
    laptop.shutdown().await?;
    tablet.shutdown().await?;
    Ok(())
}
