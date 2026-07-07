//! Three devices, two identities, a partial overlap — and no founder.
//!
//! Phone provisions Alice-at-work (connected to Bob, one data entry) and
//! Alice-at-leisure (connected to Carol, one data entry). Laptop links into
//! both from phone's seeds. Tablet links into the work identity only — and
//! its seed and data ticket come from the **laptop**, not the founder,
//! proving a linked device is a full peer: what it replicated is sufficient
//! to link the next device. Device sets end up asymmetric (work: three,
//! leisure: two), live updates cross the three-device swarm, and the tablet
//! knows nothing of the leisure identity.

use anyhow::Result;
use data_layer::{
    link_device, provision_identity, AddrInfoOptions, DocTicket, IdentityStores, ShareMode,
    SyncNode, UnknownIssuer,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{ids, wait_connected, wait_devices, wait_entry_is, TIMEOUT};

/// Provision one identity on `phone` with its fixtures: a connection to
/// `peer` and `value` at `path` in the data namespace of `issuer`. Returns
/// the phone-side stores and the linking seed.
async fn provision_with_fixtures(
    phone: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(IdentityStores, DocTicket)> {
    let stores = provision_identity(phone).await?;
    stores.connections.connect(peer).await?;
    let author = phone.create_author().await?;
    phone.create_namespace(issuer).await?;
    phone.write(issuer, author, path, value).await?;
    let seed = stores
        .private_metadata
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    Ok((stores, seed))
}

/// Hand `issuer`'s data namespace from one node to another by ticket (data
/// discovery at linking is deferred — ADR-0009).
async fn import_data_from(from: &SyncNode, to: &mut SyncNode, issuer: PdnId) -> Result<()> {
    let ticket = from
        .share_ticket(issuer, ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    to.import_namespace(issuer, ticket).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn three_devices_two_identities() -> Result<()> {
    let path = EntryPath::new("k")?;

    let mut phone = SyncNode::spawn().await?;
    let mut laptop = SyncNode::spawn().await?;
    let mut tablet = SyncNode::spawn().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let tablet_id = tablet.node_id();

    // Phone provisions both identities, each with a connection and one entry.
    let (work_phone, work_seed) =
        provision_with_fixtures(&mut phone, ids::ALICE_AT_WORK, ids::BOB, &path, b"work").await?;
    let (_leisure_phone, leisure_seed) = provision_with_fixtures(
        &mut phone,
        ids::ALICE_AT_LEISURE,
        ids::CAROL,
        &path,
        b"leisure",
    )
    .await?;

    // Laptop links into both identities from phone's seeds and imports the
    // work data namespace (data discovery at linking is deferred, ADR-0009).
    let work_laptop = link_device(&mut laptop, work_seed, TIMEOUT).await?;
    let leisure_laptop = link_device(&mut laptop, leisure_seed, TIMEOUT).await?;
    import_data_from(&phone, &mut laptop, ids::ALICE_AT_WORK).await?;
    assert!(
        wait_entry_is(&laptop, ids::ALICE_AT_WORK, &path, b"work").await?,
        "work data did not reach laptop"
    );

    // Tablet links into work only — seed and data ticket issued by the LAPTOP.
    let tablet_seed = work_laptop
        .private_metadata
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let work_tablet = link_device(&mut tablet, tablet_seed, TIMEOUT).await?;
    import_data_from(&laptop, &mut tablet, ids::ALICE_AT_WORK).await?;

    // Transitive catch-up: state authored on phone reaches the tablet
    // through stores it obtained via the laptop.
    assert!(
        wait_connected(&work_tablet.connections, ids::BOB, true).await?,
        "the Bob connection did not reach the tablet"
    );
    assert!(
        wait_entry_is(&tablet, ids::ALICE_AT_WORK, &path, b"work").await?,
        "work data did not reach the tablet"
    );

    // Live through the three-device swarm: a fresh connection on phone.
    work_phone.connections.connect(ids::DAVE).await?;
    assert!(
        wait_connected(&work_tablet.connections, ids::DAVE, true).await?,
        "a live work update did not reach the tablet"
    );

    // The work device set converges to all three — on the founder too.
    let all = [phone_id, laptop_id, tablet_id];
    assert!(
        wait_devices(&work_tablet.private_metadata, &all).await?,
        "the tablet's work device set is incomplete"
    );
    assert!(
        wait_devices(&work_phone.private_metadata, &all).await?,
        "the phone's work device set is incomplete"
    );

    // The leisure device set stays at two: the tablet is not in it.
    assert!(
        wait_devices(&leisure_laptop.private_metadata, &[phone_id, laptop_id]).await?,
        "the leisure device set did not converge"
    );
    assert!(
        !leisure_laptop
            .private_metadata
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
