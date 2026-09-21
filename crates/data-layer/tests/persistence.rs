//! Directory-configured nodes: what a node keeps on disk and what a node
//! spawned on the same directory holds again — with no peer running, so
//! nothing can have arrived over the network.
//!
//! These scenarios are the only ones in the crate that touch a filesystem;
//! every other suite runs on memory, named at its spawn helper.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, DirectoryHeld, PrivateMetadataStore, ShareMode, SpawnOptions, SyncNode,
};
use pdn_types::EntryPath;
use test_utils::{host_identity, ids};

/// Spawn a node on `dir` — the directory-configured counterpart of the
/// suites' `memory_node`.
async fn node_on(dir: &std::path::Path) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions::on_directory(dir)).await
}

/// The round trip with no peer running: same node id, both stores readable,
/// the data namespace re-imported from the directory's own `data` ticket
/// onto a replica the store already holds (the product path a restarted
/// runtime walks). The denial beside it: a fresh directory is a different
/// node holding none of it, and its reopen of a replica it never held
/// answers absence, not a failure to open.
#[tokio::test(flavor = "multi_thread")]
async fn a_respawned_node_reads_its_own_entries_and_keeps_its_id() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;
    let payload = b"alice@example.org";

    let first = node_on(dir.path()).await?;
    let first_id = first.node_id();

    let directory = host_identity(&first, ids::ALICE).await?;
    let author = first.default_author(ids::ALICE)?;
    let directory_namespace = directory.namespace();
    first.create_namespace(ids::ALICE, ids::ALICE).await?;
    first
        .write(ids::ALICE, ids::ALICE, author, &path, payload)
        .await?;
    let data_ticket = first
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::Addresses,
        )
        .await?;
    directory.put_ticket("data", &data_ticket).await?;
    first.shutdown().await?;
    drop(directory);
    drop(first);

    let second = node_on(dir.path()).await?;
    assert_eq!(
        second.node_id(),
        first_id,
        "the node id must come from the stored key"
    );
    second.provision_identity(ids::ALICE).await?;
    let reopened = PrivateMetadataStore::open(&second, ids::ALICE, directory_namespace)
        .await?
        .expect("the respawned store must still hold the directory replica");
    second.host_identity(ids::ALICE, &reopened)?;
    assert!(
        reopened.list_devices().await?.contains(&first_id),
        "the directory's device set must survive the respawn"
    );
    let stored_ticket = reopened
        .get_ticket("data")
        .await?
        .expect("the data ticket and its payload are local");
    let _rebound = second
        .import_namespace(ids::ALICE, ids::ALICE, stored_ticket)
        .await?;
    assert_eq!(
        second.read(ids::ALICE, ids::ALICE, &path).await?.as_deref(),
        Some(payload.as_slice()),
        "the entry and its payload must come back from the directory alone"
    );

    // Denial: a fresh directory is a different node with none of the state.
    let fresh_dir = tempfile::tempdir()?;
    let fresh = node_on(fresh_dir.path()).await?;
    assert_ne!(
        fresh.node_id(),
        first_id,
        "a fresh directory must be a different node"
    );
    fresh.provision_identity(ids::ALICE).await?;
    assert!(
        fresh.read(ids::ALICE, ids::ALICE, &path).await.is_err(),
        "a fresh node must refuse the issuer as unknown"
    );
    assert!(
        PrivateMetadataStore::open(&fresh, ids::ALICE, directory_namespace)
            .await?
            .is_none(),
        "a replica this store never held must be reported absent, not as a failure to open"
    );
    fresh.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

/// One author per hosted identity, persisted with that identity's stores:
/// a path rewritten after a restart replaces its predecessor instead of
/// accreting beside it under a second author. Every latest-wins read hides
/// the difference, so the assertion counts live records across authors.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_after_a_restart_keeps_one_live_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let first = node_on(dir.path()).await?;
    let directory = host_identity(&first, ids::ALICE).await?;
    let directory_namespace = directory.namespace();
    first.create_namespace(ids::ALICE, ids::ALICE).await?;
    let author = first.default_author(ids::ALICE)?;
    first
        .write(ids::ALICE, ids::ALICE, author, &path, b"before")
        .await?;
    let ticket = first
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Write,
            AddrInfoOptions::Addresses,
        )
        .await?;
    directory.put_ticket("data", &ticket).await?;
    first.shutdown().await?;
    drop(directory);
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    let reopened = PrivateMetadataStore::open(&second, ids::ALICE, directory_namespace)
        .await?
        .expect("the respawned store must still hold the directory replica");
    second.host_identity(ids::ALICE, &reopened)?;
    let stored_ticket = reopened
        .get_ticket("data")
        .await?
        .expect("the data ticket and its payload are local");
    let _rebound = second
        .import_namespace(ids::ALICE, ids::ALICE, stored_ticket)
        .await?;
    let author = second.default_author(ids::ALICE)?;
    second
        .write(ids::ALICE, ids::ALICE, author, &path, b"after")
        .await?;
    assert_eq!(
        second.read(ids::ALICE, ids::ALICE, &path).await?.as_deref(),
        Some(b"after".as_slice())
    );
    assert_eq!(
        second
            .live_record_count(ids::ALICE, ids::ALICE, &path)
            .await?,
        1,
        "a rewrite as the persisted author must replace, not accrete"
    );

    // Denial: a record under a separate author accretes, so the count
    // above is an instrument and not a constant.
    let other_author = second.create_author(ids::ALICE).await?;
    second
        .write(ids::ALICE, ids::ALICE, other_author, &path, b"elsewhere")
        .await?;
    assert_eq!(
        second
            .live_record_count(ids::ALICE, ids::ALICE, &path)
            .await?,
        2,
        "a second author's record must be countable, or the assertion above proves nothing"
    );
    second.shutdown().await?;
    Ok(())
}

/// Two identities of one node each keep their own author across a restart:
/// what a counterparty or a cell binds to an identity on this device is
/// that identity's author, not the node's.
#[tokio::test(flavor = "multi_thread")]
async fn each_identity_keeps_its_own_author_across_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = node_on(dir.path()).await?;
    let work_dir = host_identity(&first, ids::ALICE_AT_WORK).await?;
    let leisure_dir = host_identity(&first, ids::ALICE_AT_LEISURE).await?;
    let (work_namespace, leisure_namespace) = (work_dir.namespace(), leisure_dir.namespace());
    let work_author = first.default_author(ids::ALICE_AT_WORK)?;
    let leisure_author = first.default_author(ids::ALICE_AT_LEISURE)?;
    assert_ne!(
        work_author, leisure_author,
        "two identities of one node wrote under one author"
    );
    first.shutdown().await?;
    drop(work_dir);
    drop(leisure_dir);
    drop(first);

    let second = node_on(dir.path()).await?;
    for (identity, namespace, before) in [
        (ids::ALICE_AT_WORK, work_namespace, work_author),
        (ids::ALICE_AT_LEISURE, leisure_namespace, leisure_author),
    ] {
        second.provision_identity(identity).await?;
        let reopened = PrivateMetadataStore::open(&second, identity, namespace)
            .await?
            .expect("the respawned store must still hold the directory replica");
        second.host_identity(identity, &reopened)?;
        assert_eq!(
            second.default_author(identity)?,
            before,
            "an identity came back writing as another author"
        );
    }
    second.shutdown().await?;
    Ok(())
}

/// A device withdrawn after a restart stays absent, although the record it
/// withdraws was written by another author: this node's tombstone leaves
/// that record live in the replica, and the set reads the device as absent
/// only because the latest-per-key collapse sees the tombstone before empty
/// entries are excluded. The restart is load-bearing twice: the tombstone
/// must be written by the author this identity had before it, and the
/// buried record must have survived on disk.
///
/// The second author is a sibling device of the same identity, which is
/// where two authors on one connection metadata store come from. Each
/// side's counterpart half is created locally: registering the pair is
/// what arms a connection metadata store's ticket bound (Invariant 3),
/// and only the `own` half is this scenario's subject.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_withdrawn_after_a_restart_stays_absent() -> Result<()> {
    use std::str::FromStr as _;

    use data_layer::ConnectionMetadataStore;
    let dir = tempfile::tempdir()?;
    // A device record must resolve into an endpoint id to list, so the
    // withdrawn device is a real key that never runs.
    let withdrawn = pdn_types::NodeId::from_bytes(
        *iroh::PublicKey::from_str(
            "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6",
        )
        .expect("valid test key")
        .as_bytes(),
    );

    let first = node_on(dir.path()).await?;
    let directory = host_identity(&first, ids::ALICE).await?;
    let own_node = first.node_id();
    let cms = ConnectionMetadataStore::create(&first, ids::ALICE).await?;
    let counterpart = ConnectionMetadataStore::create(&first, ids::ALICE).await?;
    first.host_connection(ids::ALICE, ids::BOB, &cms, &counterpart)?;
    cms.ensure_device_published(own_node).await?;
    let ticket = cms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let directory_ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;

    // A sibling device of the same identity writes the withdrawn device's
    // record, so it stands under another author than the tombstone below.
    let sibling = test_utils::memory_node().await?;
    let sibling_dir = test_utils::join_identity(&sibling, ids::ALICE, directory_ticket).await?;
    directory.add_device(sibling.node_id()).await?;
    sibling_dir.add_device(sibling.node_id()).await?;
    let sibling_cms = ConnectionMetadataStore::import(&sibling, ids::ALICE, ticket.clone()).await?;
    let sibling_counterpart = ConnectionMetadataStore::create(&sibling, ids::ALICE).await?;
    sibling.host_connection(ids::ALICE, ids::BOB, &sibling_cms, &sibling_counterpart)?;
    sibling_cms.ensure_device_published(withdrawn).await?;
    assert!(
        test_utils::eventually(|| async {
            Ok(cms.published_devices().await?.contains(&withdrawn))
        })
        .await?,
        "the sibling's record never reached this replica"
    );
    sibling.shutdown().await?;
    drop(sibling_cms);
    drop(sibling);
    first.shutdown().await?;
    drop(cms);
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    let cms = ConnectionMetadataStore::import(&second, ids::ALICE, ticket).await?;
    cms.withdraw_device(withdrawn).await?;
    let published = cms.published_devices().await?;
    assert!(
        published.contains(&own_node),
        "the surviving device record must still read: {published:?}"
    );
    assert!(
        !published.contains(&withdrawn),
        "a device withdrawn after the restart must be absent from the set"
    );
    second.shutdown().await?;
    Ok(())
}

/// One running node per directory: the second spawn is refused with an
/// error naming the directory, and the running node is untouched by the
/// refused start — the refusal is a named condition, not a lock error that
/// reads as a corrupt store.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_node_on_a_held_directory_is_refused_by_name() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = EntryPath::new("contact/email")?;

    let running = node_on(dir.path()).await?;
    let _directory = host_identity(&running, ids::ALICE).await?;
    running.create_namespace(ids::ALICE, ids::ALICE).await?;
    let author = running.default_author(ids::ALICE)?;

    let err = node_on(dir.path())
        .await
        .expect_err("a held directory must refuse a second node");
    let Some(held) = err.downcast_ref::<DirectoryHeld>() else {
        anyhow::bail!("expected DirectoryHeld, got: {err:#}");
    };
    assert_eq!(held.directory, dir.path());

    // The running node is unaffected by the refused start.
    running
        .write(ids::ALICE, ids::ALICE, author, &path, b"still mine")
        .await?;
    assert_eq!(
        running
            .read(ids::ALICE, ids::ALICE, &path)
            .await?
            .as_deref(),
        Some(b"still mine".as_slice())
    );
    running.shutdown().await?;
    Ok(())
}

/// A key file that cannot be parsed stops the start with an error naming
/// it, and the file is left exactly as it was — never a regenerated key,
/// which would silently change the node id the directory's records and
/// tickets all name.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_key_file_stops_the_start_and_is_not_replaced() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    first.shutdown().await?;
    drop(first);

    // Truncate the key: present, unparseable.
    let key_path = dir.path().join("node.key");
    std::fs::write(&key_path, b"not a key")?;

    let err = node_on(dir.path())
        .await
        .expect_err("a malformed key must stop the start");
    assert!(
        format!("{err:#}").contains("node.key"),
        "the refusal must name the key file: {err:#}"
    );
    assert_eq!(
        std::fs::read(&key_path)?,
        b"not a key",
        "the malformed key must not be replaced with a fresh one"
    );
    Ok(())
}

/// A staging file left by a start that died while minting the key does not
/// block the next one, and is gone afterwards. The staged name is fixed
/// rather than derived from the process id, so on a container — always
/// pid 1 — a leftover would otherwise stop every later start. The denial
/// beside it: the committed key is not a leftover and is read back, not
/// minted over.
#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_key_staging_file_does_not_block_the_start() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let staged = dir.path().join("node.key.tmp");
    std::fs::create_dir_all(dir.path())?;
    std::fs::write(&staged, b"half a key from a start that died")?;

    let node = node_on(dir.path()).await?;
    let id = node.node_id();
    assert!(
        dir.path().join("node.key").exists(),
        "the interrupted start's leftover must not stop the key from being minted"
    );
    assert!(
        !staged.exists(),
        "the staging file must not survive the start that used it"
    );
    node.shutdown().await?;
    drop(node);

    // Denial: the committed key is not a leftover — a restart reads it.
    let again = node_on(dir.path()).await?;
    assert_eq!(
        again.node_id(),
        id,
        "the committed key must be read back, not minted again"
    );
    again.shutdown().await?;
    Ok(())
}

/// A device record rewritten after a restart replaces its predecessor
/// instead of accreting beside it under a second author: the directory
/// writes with its identity's one author — the half a store that minted
/// its own author would break invisibly, since every product read is
/// latest-wins. The denial beside it: a record under a separate author does
/// accrete, so the count is an instrument and not a constant.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_rewritten_after_a_restart_keeps_one_live_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let device = first.node_id();
    let directory = host_identity(&first, ids::ALICE).await?;
    let namespace = directory.namespace();
    assert_eq!(
        directory.live_device_record_count(device).await?,
        1,
        "the first write must leave one record"
    );
    first.shutdown().await?;
    drop(directory);
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    let reopened = PrivateMetadataStore::open(&second, ids::ALICE, namespace)
        .await?
        .expect("the respawned store must still hold the directory replica");
    second.host_identity(ids::ALICE, &reopened)?;
    reopened.add_device(device).await?;
    assert_eq!(
        reopened.live_device_record_count(device).await?,
        1,
        "the rewrite after the restart must replace the record, not accrete beside it"
    );

    // Denial: a record written under another author accretes — the count
    // above is not one by construction.
    let other_author = second.create_author(ids::ALICE).await?;
    reopened
        .add_device_as_for_test(device, other_author)
        .await?;
    assert_eq!(
        reopened.live_device_record_count(device).await?,
        2,
        "a second author's record must be countable, or the assertion above proves nothing"
    );
    second.shutdown().await?;
    Ok(())
}
