//! Three devices, two identities, a partial overlap: a device that
//! replicated a store is a full peer, and what it holds is sufficient to
//! bring up the next device.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, DocTicket, PrivateMetadataStore, ShareMode, SyncNode, UnknownIssuer,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{
    host_identity, ids, join_identity, memory_node, wait_connected, wait_devices, wait_entry_is,
};

/// Bring one identity up on `phone` with its fixtures: a PMS with the
/// phone registered and a connection to `peer`, plus `value` at `path` in
/// the data namespace of `issuer`. Returns the phone-side PMS and its
/// write ticket.
async fn provision_with_fixtures(
    phone: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(PrivateMetadataStore, DocTicket)> {
    let pms = host_identity(phone, issuer).await?;
    pms.connect(peer).await?;
    let author = phone.default_author(issuer)?;
    phone.create_namespace(issuer, issuer).await?;
    phone.write(issuer, issuer, author, path, value).await?;
    let ticket = pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    Ok((pms, ticket))
}

/// Bring the identity behind `ticket` up on `node`, as device linking does
/// at the store level: bring up the identity's stores, import the
/// PMS and join the device set.
async fn join_from(
    node: &SyncNode,
    identity: PdnId,
    ticket: DocTicket,
) -> Result<PrivateMetadataStore> {
    let pms = join_identity(node, identity, ticket).await?;
    pms.add_device(node.node_id()).await?;
    Ok(pms)
}

/// Hand `issuer`'s data namespace from one node to another by ticket.
async fn import_data_from(from: &SyncNode, to: &mut SyncNode, issuer: PdnId) -> Result<()> {
    let ticket = from
        .share_ticket(
            issuer,
            issuer,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    to.import_namespace(issuer, issuer, ticket).await?;
    Ok(())
}

/// The tablet joins the work identity from tickets the laptop minted, not
/// the first device: state authored on the phone reaches it transitively, a
/// live update crosses the three-device swarm, the device sets end up
/// asymmetric (work: three, leisure: two), and the tablet knows nothing of
/// the leisure identity.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, three devices and two identities in one place
async fn three_devices_two_identities() -> Result<()> {
    let path = EntryPath::new("affiliation/group")?;

    let mut phone = memory_node().await?;
    let mut laptop = memory_node().await?;
    let mut tablet = memory_node().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let tablet_id = tablet.node_id();

    // Phone brings up both identities, each with a connection and one entry.
    let (work_phone_pms, work_ticket) = provision_with_fixtures(
        &mut phone,
        ids::ALICE_AT_WORK,
        ids::BOB,
        &path,
        b"Acme Engineering",
    )
    .await?;
    let (_leisure_phone_pms, leisure_ticket) = provision_with_fixtures(
        &mut phone,
        ids::ALICE_AT_LEISURE,
        ids::CAROL,
        &path,
        b"Boston Bridge Club",
    )
    .await?;

    // Laptop joins both identities from phone's tickets and imports the
    // work data namespace.
    let work_laptop_pms = join_from(&laptop, ids::ALICE_AT_WORK, work_ticket).await?;
    let leisure_laptop_pms = join_from(&laptop, ids::ALICE_AT_LEISURE, leisure_ticket).await?;
    import_data_from(&phone, &mut laptop, ids::ALICE_AT_WORK).await?;
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            &path,
            b"Acme Engineering"
        )
        .await?,
        "work data did not reach laptop"
    );

    // Tablet joins work only — PMS and data tickets issued by the
    // LAPTOP.
    let tablet_ticket = work_laptop_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let work_tablet_pms = join_from(&tablet, ids::ALICE_AT_WORK, tablet_ticket).await?;
    import_data_from(&laptop, &mut tablet, ids::ALICE_AT_WORK).await?;

    // Transitive catch-up: state authored on phone reaches the tablet
    // through stores it obtained via the laptop.
    assert!(
        wait_connected(&work_tablet_pms, ids::BOB, true).await?,
        "the Bob connection did not reach the tablet"
    );
    assert!(
        wait_entry_is(
            &tablet,
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            &path,
            b"Acme Engineering"
        )
        .await?,
        "work data did not reach the tablet"
    );

    // Live through the three-device swarm: a fresh connection on phone.
    work_phone_pms.connect(ids::DAVE).await?;
    assert!(
        wait_connected(&work_tablet_pms, ids::DAVE, true).await?,
        "a live work update did not reach the tablet"
    );

    // The work device set converges to all three — on the first device too.
    let all = [phone_id, laptop_id, tablet_id];
    assert!(
        wait_devices(&work_tablet_pms, &all).await?,
        "the tablet's work device set is incomplete"
    );
    assert!(
        wait_devices(&work_phone_pms, &all).await?,
        "the phone's work device set is incomplete"
    );

    // The leisure device set stays at two: the tablet is not in it.
    assert!(
        wait_devices(&leisure_laptop_pms, &[phone_id, laptop_id]).await?,
        "the leisure device set did not converge"
    );
    assert!(
        !leisure_laptop_pms
            .list_devices()
            .await?
            .contains(&tablet_id),
        "the tablet leaked into the leisure device set"
    );

    // And the tablet knows nothing of leisure: read as the identity it
    // does host, the leisure issuer resolves to nothing there.
    let err = tablet
        .read(ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE, &path)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    phone.shutdown().await?;
    laptop.shutdown().await?;
    tablet.shutdown().await?;
    Ok(())
}
