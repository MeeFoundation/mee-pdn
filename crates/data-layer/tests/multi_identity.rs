//! One pair of devices hosts two identities side by side — Alice-at-work
//! and Alice-at-leisure, each a `PdnId` with its own store set. The phone
//! brings up both; the laptop joins each by a separate import of that
//! identity's directory ticket.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, AuthorId, DocTicket, PrivateMetadataStore, ShareMode, SyncNode, UnknownIssuer,
};
use pdn_types::{EntryPath, PdnId};
use test_utils::{
    host_identity, ids, join_identity, memory_node, wait_connected, wait_devices, wait_entry_is,
};

/// Bring one identity up on `node` and lay down its test fixtures: a
/// directory with the node registered and a connection to `peer`, and a data
/// namespace of `issuer` with `value` written at `path`. Returns the
/// phone-side directory handle, the author for further data writes, the
/// directory's write ticket, and the data-namespace ticket.
async fn provision_identity_with_data(
    node: &mut SyncNode,
    issuer: PdnId,
    peer: PdnId,
    path: &EntryPath,
    value: &[u8],
) -> Result<(PrivateMetadataStore, AuthorId, DocTicket, DocTicket)> {
    let directory = host_identity(node, issuer).await?;
    directory.connect(peer).await?;
    let ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let author = node.default_author(issuer)?;
    node.create_namespace(issuer, issuer).await?;
    node.write(issuer, issuer, author, path, value).await?;
    let data_ticket = node
        .share_ticket(
            issuer,
            issuer,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;

    Ok((directory, author, ticket, data_ticket))
}

/// Both identities' store sets replicate to the laptop, each by its own
/// import; joining the first brings nothing of the second, the two stay
/// isolated, and the first keeps operating after the second joins.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, both identities' whole lifetime in one place
async fn multi_identity_two_devices() -> Result<()> {
    // The same path in both data namespaces, a different value in each —
    // mixed-up stores would surface as the wrong value.
    let path = EntryPath::new("affiliation/group")?;

    // Phone hosts both identities' store sets side by side.
    let mut phone = memory_node().await?;
    let laptop = memory_node().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let (work_phone_dir, work_author, work_ticket, work_data) = provision_identity_with_data(
        &mut phone,
        ids::ALICE_AT_WORK,
        ids::BOB,
        &path,
        b"Acme Engineering",
    )
    .await?;
    let (_leisure_phone_dir, _leisure_author, leisure_ticket, leisure_data) =
        provision_identity_with_data(
            &mut phone,
            ids::ALICE_AT_LEISURE,
            ids::CAROL,
            &path,
            b"Boston Bridge Club",
        )
        .await?;

    // Join the laptop into the work identity only.
    let laptop_work_dir = join_identity(&laptop, ids::ALICE_AT_WORK, work_ticket).await?;
    laptop_work_dir.add_device(laptop.node_id()).await?;
    assert!(
        wait_connected(&laptop_work_dir, ids::BOB, true).await?,
        "work connections did not replicate to laptop"
    );

    // Isolation: nothing of the leisure identity arrived through that act —
    // its peer is not in the work directory, and its namespace is unknown
    // here (specifically unknown, not just any error).
    assert!(!laptop_work_dir.is_connected(ids::CAROL).await?);
    let err = laptop
        .read(ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE, &path)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    // Second identity joins the already-joined node by its own act.
    let laptop_leisure_dir = join_identity(&laptop, ids::ALICE_AT_LEISURE, leisure_ticket).await?;
    laptop_leisure_dir.add_device(laptop.node_id()).await?;
    assert!(
        wait_connected(&laptop_leisure_dir, ids::CAROL, true).await?,
        "leisure connections did not replicate to laptop"
    );
    assert!(!laptop_leisure_dir.is_connected(ids::BOB).await?);
    assert!(
        wait_devices(&laptop_leisure_dir, &[phone_id, laptop_id]).await?,
        "leisure device set did not converge on laptop"
    );

    // Both identities' data namespaces replicate, each under its own issuer.
    laptop
        .import_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, work_data)
        .await?;
    laptop
        .import_namespace(ids::ALICE_AT_LEISURE, ids::ALICE_AT_LEISURE, leisure_data)
        .await?;
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            &path,
            b"Acme Engineering"
        )
        .await?,
        "work data did not replicate to laptop"
    );
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_LEISURE,
            ids::ALICE_AT_LEISURE,
            &path,
            b"Boston Bridge Club"
        )
        .await?,
        "leisure data did not replicate to laptop"
    );

    // The first identity keeps operating after the second joined: a fresh
    // connection and a fresh data write (an LWW overwrite of the same path)
    // both still reach the laptop.
    work_phone_dir.connect(ids::DAVE).await?;
    assert!(
        wait_connected(&laptop_work_dir, ids::DAVE, true).await?,
        "work connections stopped replicating after the second identity joined"
    );
    phone
        .write(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            work_author,
            &path,
            b"Acme Research",
        )
        .await?;
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            &path,
            b"Acme Research"
        )
        .await?,
        "work data stopped replicating after the second identity joined"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// Forgetting an imported data namespace makes its issuer unknown again —
/// the distinguishable refusal, not a storage error against a dropped
/// replica — while the co-located identity's own replica stays
/// addressable.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the forget and the co-hosted identity in one place
async fn forgetting_a_namespace_unregisters_its_issuer() -> Result<()> {
    let path = EntryPath::new("affiliation/group")?;

    // Phone issues both namespaces; the laptop imports both.
    let mut phone = memory_node().await?;
    let laptop = memory_node().await?;
    let (work_dir, _work_author, work_ticket, work_data) = provision_identity_with_data(
        &mut phone,
        ids::ALICE_AT_WORK,
        ids::BOB,
        &path,
        b"Acme Engineering",
    )
    .await?;
    let (leisure_dir, _leisure_author, leisure_ticket, leisure_data) =
        provision_identity_with_data(
            &mut phone,
            ids::ALICE_AT_LEISURE,
            ids::CAROL,
            &path,
            b"Boston Bridge Club",
        )
        .await?;
    // The laptop is a device of both identities, so the phone serves it
    // both replicas: the product's own way to a device replication.
    let _laptop_work_dir = join_identity(&laptop, ids::ALICE_AT_WORK, work_ticket).await?;
    let _laptop_leisure_dir = join_identity(&laptop, ids::ALICE_AT_LEISURE, leisure_ticket).await?;
    work_dir.add_device(laptop.node_id()).await?;
    leisure_dir.add_device(laptop.node_id()).await?;
    laptop
        .import_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, work_data)
        .await?;
    laptop
        .import_namespace(ids::ALICE_AT_LEISURE, ids::ALICE_AT_LEISURE, leisure_data)
        .await?;
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            &path,
            b"Acme Engineering"
        )
        .await?,
        "work data did not replicate before the forget"
    );

    // Forget one issuer: reads, writes, and lists under it refuse as
    // specifically unknown — exactly as before the import.
    laptop
        .forget_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = laptop.default_author(ids::ALICE_AT_WORK)?;
    let read_err = laptop
        .read(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, &path)
        .await
        .unwrap_err();
    assert!(read_err.downcast_ref::<UnknownIssuer>().is_some());
    let write_err = laptop
        .write(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            author,
            &path,
            b"residue",
        )
        .await
        .unwrap_err();
    assert!(write_err.downcast_ref::<UnknownIssuer>().is_some());
    let list_err = laptop
        .list(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, None)
        .await
        .unwrap_err();
    assert!(list_err.downcast_ref::<UnknownIssuer>().is_some());

    // Forgetting an issuer that is (now) unknown refuses the same way.
    let again = laptop
        .forget_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await
        .unwrap_err();
    assert!(again.downcast_ref::<UnknownIssuer>().is_some());

    // The co-hosted identity is untouched: its entries remain readable.
    assert!(
        wait_entry_is(
            &laptop,
            ids::ALICE_AT_LEISURE,
            ids::ALICE_AT_LEISURE,
            &path,
            b"Boston Bridge Club"
        )
        .await?,
        "the co-hosted identity must stay addressable after the forget"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
