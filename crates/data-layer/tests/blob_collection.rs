//! Blob collection over a node's one blob store: a payload stays while a
//! replica of any identity the node hosts references it and leaves at the
//! first run after none does, a forgotten replica freeing its payloads with
//! no removal of its own.

use std::time::Duration;

use anyhow::Result;
use data_layer::{CellStore, SpawnOptions, SyncNode};
use iroh_blobs::Hash;
use pdn_types::{EntryPath, RecordId, RecordRef};
use test_utils::{
    cell::{device_of, found, host, invite, place_claim, reads, tickets, Person},
    eventually,
};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

async fn node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        cell_reconcile_interval: QUIET,
        blob_collection_interval: Duration::from_millis(200),
        ..SpawnOptions::memory()
    })
    .await
}

/// `member`'s claim at the id `seed` names, its payload `payload`.
async fn claim(
    node: &SyncNode,
    member: &Person,
    cell: pdn_types::CellId,
    seed: u8,
    payload: &[u8],
) -> Result<RecordRef> {
    let key = data_layer::RecordKey::Claim {
        member: member.id,
        id: RecordId::from_bytes([seed; 16]),
        mseq: data_layer::Seq::FIRST,
    };
    node.write_cell_entry(
        member.id,
        cell,
        CellStore::Records,
        &key.to_bytes(),
        payload,
    )
    .await?;
    Ok(key.record())
}

/// The payloads a forgotten record store alone referenced leave the node at
/// the next run. Denied: one the identity's own data namespace references
/// too stays, and reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_no_replica_references_is_removed() -> Result<()> {
    let phone = node().await?;
    let (carol, _) = host(&phone).await?;
    let cell = found(&phone, &carol).await?;
    claim(&phone, &carol, cell, 1, b"lease scan").await?;
    claim(&phone, &carol, cell, 2, b"photo").await?;
    phone.create_namespace(carol.id, carol.id).await?;
    let photo = EntryPath::new("photos/beach")?;
    phone
        .write(
            carol.id,
            carol.id,
            phone.default_author(carol.id)?,
            &photo,
            b"photo",
        )
        .await?;
    let (scan, picture) = (Hash::new(b"lease scan"), Hash::new(b"photo"));
    for hash in [scan, picture] {
        assert!(phone.holds_payload(hash).await?);
    }

    phone.forget_cell(carol.id, cell).await?;
    assert!(
        eventually(|| async { Ok(!phone.holds_payload(scan).await?) }).await?,
        "the payload no replica references stayed"
    );
    // Denied: the payload Carol's data namespace references.
    assert!(phone.holds_payload(picture).await?);
    assert_eq!(
        phone.read(carol.id, carol.id, &photo).await?.as_deref(),
        Some(&b"photo"[..])
    );

    phone.shutdown().await?;
    Ok(())
}

/// A payload a co-located identity's replica still references stays when
/// the other identity forgets its replica, and that identity reads it.
///
/// A payload only the forgetting identity's data namespace referenced,
/// forgotten beside the cell, orders the assertion after a run that
/// removed something.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_a_co_located_identity_references_stays() -> Result<()> {
    let tablet = node().await?;
    let (leisure, _) = host(&tablet).await?;
    let (work, _) = host(&tablet).await?;
    let cell = found(&tablet, &leisure).await?;
    invite(
        &tablet,
        &leisure,
        cell,
        &work,
        vec![device_of(&tablet, &work)?],
    )
    .await?;
    tablet
        .import_cell(work.id, cell, tickets(&tablet, &leisure, cell).await?)
        .await?;
    let shared = place_claim(&tablet, &leisure, cell, 1).await?;
    assert!(reads(&tablet, work.id, cell, shared).await?);
    tablet.create_namespace(work.id, work.id).await?;
    let notes = EntryPath::new("notes/today")?;
    tablet
        .write(
            work.id,
            work.id,
            tablet.default_author(work.id)?,
            &notes,
            b"only work's",
        )
        .await?;
    let (sentinel, claim) = (Hash::new(b"only work's"), Hash::new(b"claim"));

    tablet.forget_cell(work.id, cell).await?;
    tablet.forget_namespace(work.id, work.id).await?;
    assert!(eventually(|| async { Ok(!tablet.holds_payload(sentinel).await?) }).await?);
    assert!(
        tablet.holds_payload(claim).await?,
        "a payload a co-located identity references was removed"
    );
    assert!(reads(&tablet, leisure.id, cell, shared).await?);

    tablet.shutdown().await?;
    Ok(())
}

/// After a restart, a node on a storage directory removes nothing until its
/// host has hosted its identities again and lets collection start, and
/// collects from then on.
///
/// Nothing is collected before the start, so no removal can order the
/// absence of one; the wait spans several runs at their interval.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_removes_nothing_before_its_host_lets_collection_start() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let on_dir = || {
        SyncNode::spawn(SpawnOptions {
            reconcile_interval: QUIET,
            cell_reconcile_interval: QUIET,
            blob_collection_interval: Duration::from_millis(200),
            ..SpawnOptions::on_directory(dir.path())
        })
    };
    let first = on_dir().await?;
    let carol = Person::generate();
    let directory = test_utils::host_identity(&first, carol.id).await?;
    first
        .record_hosting(carol.id, directory.namespace())
        .await?;
    first.create_namespace(carol.id, carol.id).await?;
    let photo = EntryPath::new("photos/beach")?;
    first
        .write(
            carol.id,
            carol.id,
            first.default_author(carol.id)?,
            &photo,
            b"photo",
        )
        .await?;
    first.shutdown().await?;
    drop(first);

    let second = on_dir().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        second.holds_payload(Hash::new(b"photo")).await?,
        "a payload was removed before the host let collection start"
    );
    second.provision_identity(carol.id).await?;
    let reopened = data_layer::PrivateMetadataStore::open(&second, carol.id, directory.namespace())
        .await?
        .expect("the directory survives the restart");
    second.host_identity(carol.id, &reopened)?;
    second.start_blob_collection();
    // Collection runs from the start on: a payload nothing references goes.
    let stray = second.add_stray_payload_for_test(b"stray").await?;
    assert!(eventually(|| async { Ok(!second.holds_payload(stray).await?) }).await?);
    assert!(second.holds_payload(Hash::new(b"photo")).await?);
    second.shutdown().await?;
    Ok(())
}
