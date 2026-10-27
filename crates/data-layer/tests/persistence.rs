//! Directory-configured nodes: what a node keeps on disk and what a node
//! spawned on the same directory holds again — with no peer running, so
//! nothing can have arrived over the network.
//!
//! These scenarios are the only ones in the crate that touch a filesystem;
//! every other suite runs on memory, named at its spawn helper.

use anyhow::Result;
use data_layer::private_metadata::PENDING_DEVICE_TTL;
use data_layer::{
    pod_ticket_kind, AddrInfoOptions, AuthorId, DirectoryHeld, NamespaceId, OpId, PodStore,
    PodTickets, PrivateMetadataStore, RecordKey, RecordedHosting, Seq, SpawnOptions, SyncNode,
};
use pdn_types::{PdnId, PodId, RecordId, RecordKind, RecordRef};
use test_utils::{host_identity, ids, pod as c};

const FAMILY: PodId = PodId::from_bytes([0xfa; 16]);

/// Spawn a node on `dir` — the directory-configured counterpart of the
/// suites' `memory_node`.
async fn node_on(dir: &std::path::Path) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions::on_directory(dir)).await
}

/// Every entry a pod's record store holds under `key`, across authors:
/// what a latest-wins read collapses to one.
async fn entries_at(node: &SyncNode, holder: PdnId, pod: PodId, key: &RecordKey) -> Result<usize> {
    let key = key.to_bytes();
    Ok(node
        .pod_record_view(holder, pod)
        .await?
        .verdicts()
        .filter(|(entry, _verdict)| entry.key == key)
        .count())
}

/// Both of `pod`'s tickets, as `holder`'s PMS publishes them.
async fn publish_pod(pms: &PrivateMetadataStore, pod: PodId, tickets: &PodTickets) -> Result<()> {
    pms.put_ticket(
        &pod_ticket_kind(&pod, PodStore::Membership),
        &tickets.membership,
    )
    .await?;
    pms.put_ticket(&pod_ticket_kind(&pod, PodStore::Records), &tickets.records)
        .await?;
    pms.record_pod(pod, Seq::FIRST).await
}

/// The round trip with no peer running: same node id, the PMS and a pod's
/// record store readable, the pod re-imported from the tickets its PMS
/// holds onto replicas the store already holds (the product path a
/// restarted runtime walks). The denial beside it: a fresh directory is a
/// different node holding none of it, and its reopen of a replica it never
/// held answers absence, not a failure to open.
#[tokio::test(flavor = "multi_thread")]
async fn a_respawned_node_reads_its_own_entries_and_keeps_its_id() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = node_on(dir.path()).await?;
    let first_id = first.node_id();
    let (alice, pms) = c::host(&first).await?;
    let pms_namespace = pms.namespace();
    let pod = c::create(&first, &alice).await?;
    let record = c::place_claim(&first, &alice, pod, 0x01).await?;
    publish_pod(&pms, pod, &c::tickets(&first, &alice, pod).await?).await?;
    first.shutdown().await?;
    drop(pms);
    drop(first);

    let second = node_on(dir.path()).await?;
    assert_eq!(
        second.node_id(),
        first_id,
        "the node id must come from the stored key"
    );
    second.provision_identity(alice.id).await?;
    let reopened = PrivateMetadataStore::open(&second, alice.id, pms_namespace)
        .await?
        .expect("the respawned store must still hold the PMS replica");
    second.host_identity(alice.id, &reopened)?;
    assert!(
        reopened.list_devices().await?.contains(&first_id),
        "the PMS's device set must survive the respawn"
    );
    assert_eq!(reopened.held_pods().await?, [pod]);
    let ticket = |store| {
        let reopened = &reopened;
        async move {
            reopened
                .get_ticket(&pod_ticket_kind(&pod, store))
                .await?
                .ok_or_else(|| anyhow::anyhow!("the pod's ticket and its payload are local"))
        }
    };
    let tickets = PodTickets {
        membership: ticket(PodStore::Membership).await?,
        records: ticket(PodStore::Records).await?,
    };
    second.import_pod(alice.id, pod, tickets).await?;
    assert_eq!(
        second
            .read_pod_record(alice.id, pod, &record)
            .await?
            .as_deref(),
        Some(b"claim".as_slice()),
        "the record and its payload must come back from the directory alone"
    );

    // Denial: a fresh directory is a different node with none of the state.
    let fresh_dir = tempfile::tempdir()?;
    let fresh = node_on(fresh_dir.path()).await?;
    assert_ne!(
        fresh.node_id(),
        first_id,
        "a fresh directory must be a different node"
    );
    fresh.provision_identity(alice.id).await?;
    assert!(
        fresh.read_pod_record(alice.id, pod, &record).await.is_err(),
        "a fresh node must refuse the pod as unknown"
    );
    assert!(
        PrivateMetadataStore::open(&fresh, alice.id, pms_namespace)
            .await?
            .is_none(),
        "a replica this store never held must be reported absent, not as a failure to open"
    );
    fresh.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

/// After a restart, an identity's PMS opens beside a replica row of a
/// capability kind this build does not read — a read capability an earlier
/// build imported from a modified inviter's ticket. The row is written into
/// the store file directly and sorts before the PMS's own, as a walk of the
/// table would meet it first. Denied: the replica the row names fails to
/// open, rather than reading as absent.
#[tokio::test(flavor = "multi_thread")]
async fn a_replica_row_of_an_unknown_kind_leaves_the_pms_opening() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let (alice, pms) = c::host(&first).await?;
    let pms_namespace = pms.namespace();
    first.shutdown().await?;
    drop(pms);
    drop(first);

    let unread = [0u8; 32];
    let hex = alice
        .id
        .as_bytes()
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        });
    let file = dir.path().join("identities").join(hex).join("docs.redb");
    {
        let namespaces = redb::TableDefinition::<&[u8; 32], (u8, &[u8; 32])>::new("namespaces-2");
        let db = redb::Database::open(&file)?;
        let tx = db.begin_write()?;
        tx.open_table(namespaces)?.insert(&unread, (2u8, &unread))?;
        tx.commit()?;
    }

    let second = node_on(dir.path()).await?;
    second.provision_identity(alice.id).await?;
    assert!(
        PrivateMetadataStore::open(&second, alice.id, pms_namespace)
            .await?
            .is_some(),
        "the PMS must open beside a row it does not read"
    );
    // Denied: the unread row's own replica.
    assert!(
        PrivateMetadataStore::open(&second, alice.id, NamespaceId::from(unread))
            .await
            .is_err(),
        "a held replica that cannot open must fail, not read as absent"
    );
    second.shutdown().await?;
    Ok(())
}

/// One author per hosted identity, persisted with that identity's stores:
/// a pod entry rewritten after a restart replaces its predecessor instead
/// of accreting beside it under a second author. Every latest-wins read
/// hides the difference, so the assertion counts the entries at the key
/// across authors.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_after_a_restart_keeps_one_live_record() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = node_on(dir.path()).await?;
    let (alice, _pms) = c::host(&first).await?;
    let pod = c::create(&first, &alice).await?;
    let key = RecordKey::Claim {
        member: alice.id,
        id: RecordId::from_bytes([0x01; 16]),
        mseq: Seq::FIRST,
    };
    first
        .write_pod_entry(alice.id, pod, PodStore::Records, &key.to_bytes(), b"before")
        .await?;
    let tickets = c::tickets(&first, &alice, pod).await?;
    first.shutdown().await?;
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(alice.id).await?;
    second.import_pod(alice.id, pod, tickets).await?;
    second
        .write_pod_entry(alice.id, pod, PodStore::Records, &key.to_bytes(), b"after")
        .await?;
    assert_eq!(
        second
            .read_pod_record(alice.id, pod, &key.record())
            .await?
            .as_deref(),
        Some(b"after".as_slice())
    );
    assert_eq!(
        entries_at(&second, alice.id, pod, &key).await?,
        1,
        "a rewrite as the persisted author must replace, not accrete"
    );

    // Denial: an entry under a separate author accretes, so the count
    // above is an instrument and not a constant.
    let other_author = second.create_author(alice.id).await?;
    second
        .write_pod_entry_as_for_test(
            alice.id,
            pod,
            PodStore::Records,
            other_author,
            &key.to_bytes(),
            b"elsewhere",
        )
        .await?;
    assert_eq!(
        entries_at(&second, alice.id, pod, &key).await?,
        2,
        "a second author's entry must be countable, or the assertion above proves nothing"
    );
    second.shutdown().await?;
    Ok(())
}

/// Two identities of one node each keep their own author across a restart:
/// what a pod binds to an identity on this device is that identity's
/// author, not the node's.
#[tokio::test(flavor = "multi_thread")]
async fn each_identity_keeps_its_own_author_across_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = node_on(dir.path()).await?;
    let work_pms = host_identity(&first, ids::ALICE_AT_WORK).await?;
    let leisure_pms = host_identity(&first, ids::ALICE_AT_LEISURE).await?;
    let (work_namespace, leisure_namespace) = (work_pms.namespace(), leisure_pms.namespace());
    let work_author = first.default_author(ids::ALICE_AT_WORK)?;
    let leisure_author = first.default_author(ids::ALICE_AT_LEISURE)?;
    assert_ne!(
        work_author, leisure_author,
        "two identities of one node wrote under one author"
    );
    first.shutdown().await?;
    drop(work_pms);
    drop(leisure_pms);
    drop(first);

    let second = node_on(dir.path()).await?;
    for (identity, namespace, before) in [
        (ids::ALICE_AT_WORK, work_namespace, work_author),
        (ids::ALICE_AT_LEISURE, leisure_namespace, leisure_author),
    ] {
        second.provision_identity(identity).await?;
        let reopened = PrivateMetadataStore::open(&second, identity, namespace)
            .await?
            .expect("the respawned store must still hold the PMS replica");
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

/// A pending device record another device wrote, tombstoned by this
/// device's cleanup after a restart, stays absent: the tombstone leaves the
/// other author's record live in the replica, and the pending set reads the
/// device as absent only because the latest-per-key collapse sees the
/// tombstone before empty entries are excluded. The restart is
/// load-bearing: the buried record has to survive on disk.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_device_expired_after_a_restart_stays_absent() -> Result<()> {
    use std::str::FromStr as _;

    let dir = tempfile::tempdir()?;
    // A pending record must resolve into an endpoint id to list, so the
    // abandoned device is a real key that never runs.
    let abandoned = pdn_types::NodeId::from_bytes(
        *iroh::PublicKey::from_str(
            "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6",
        )
        .expect("valid test key")
        .as_bytes(),
    );

    let first = node_on(dir.path()).await?;
    let pms = host_identity(&first, ids::ALICE).await?;
    let own_node = first.node_id();
    let pms_namespace = pms.namespace();
    let pms_ticket = pms.share_ticket(AddrInfoOptions::Addresses).await?;

    // A sibling device of the same identity writes the abandoned device's
    // record, so it stands under another author than the tombstone below.
    let sibling = test_utils::memory_node().await?;
    let sibling_pms = test_utils::join_identity(&sibling, ids::ALICE, pms_ticket).await?;
    pms.add_device(sibling.node_id()).await?;
    sibling_pms.add_device(sibling.node_id()).await?;
    sibling_pms.add_pending_device(abandoned).await?;
    // Its payload too: the cleanup reads a record's age from it, and once
    // the sibling is gone nothing else holds it.
    assert!(
        test_utils::eventually(|| async {
            Ok(pms.list_pending_devices().await?.contains(&abandoned)
                && pms
                    .pending_device_written_at_for_test(abandoned)
                    .await?
                    .is_some())
        })
        .await?,
        "the sibling's record never reached this replica"
    );
    sibling.shutdown().await?;
    drop(sibling_pms);
    drop(sibling);
    first.shutdown().await?;
    drop(pms);
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    let reopened = PrivateMetadataStore::open(&second, ids::ALICE, pms_namespace)
        .await?
        .expect("the respawned store must still hold the PMS replica");
    second.host_identity(ids::ALICE, &reopened)?;
    let a_day_later = std::time::SystemTime::now() + PENDING_DEVICE_TTL;
    reopened
        .cleanup_pending_devices_at_for_test(a_day_later)
        .await?;
    let pending = reopened.list_pending_devices().await?;
    assert!(
        !pending.contains(&abandoned),
        "a pending record expired after the restart must be absent: {pending:?}"
    );
    let devices = reopened.list_devices().await?;
    assert!(
        devices.contains(&own_node),
        "the surviving device record must still read: {devices:?}"
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

    let running = node_on(dir.path()).await?;
    let pms = host_identity(&running, ids::ALICE).await?;

    let err = node_on(dir.path())
        .await
        .expect_err("a held directory must refuse a second node");
    let Some(held) = err.downcast_ref::<DirectoryHeld>() else {
        anyhow::bail!("expected DirectoryHeld, got: {err:#}");
    };
    assert_eq!(held.directory, dir.path());

    // The running node is unaffected by the refused start.
    pms.record_pod(FAMILY, Seq::FIRST).await?;
    assert_eq!(pms.pod_record(FAMILY).await?, Some((Seq::FIRST, true)));
    running.shutdown().await?;
    Ok(())
}

/// A key file that cannot be parsed stops the start with an error naming
/// it, and the file is left exactly as it was — never a regenerated key,
/// which would silently change the node id the PMS's records and
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

/// An address book that does not parse starts the node anyway, as the
/// same node, and the shutdown writes over it a book that reads back. The
/// contrast with the key file above: the key is the node, the book a hint.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_address_book_starts_the_node_and_is_replaced() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let id = first.node_id();
    first.shutdown().await?;
    drop(first);

    let book = dir.path().join("peers");
    std::fs::write(&book, b"not a book")?;
    let again = node_on(dir.path()).await?;
    assert_eq!(again.node_id(), id, "the unreadable book changed the node");
    again.shutdown().await?;
    let written: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&book)?)?;
    assert!(
        written.is_empty(),
        "a node with no peers wrote a book naming some: {written:?}"
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
/// instead of accreting beside it under a second author: the PMS
/// writes with its identity's one author — the half a store that minted
/// its own author would break invisibly, since every product read is
/// latest-wins. The denial beside it: a record under a separate author does
/// accrete, so the count is an instrument and not a constant.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_pms_rewritten_after_a_restart_keeps_one_live_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let device = first.node_id();
    let pms = host_identity(&first, ids::ALICE).await?;
    let namespace = pms.namespace();
    assert_eq!(
        pms.live_device_record_count(device).await?,
        1,
        "the first write must leave one record"
    );
    first.shutdown().await?;
    drop(pms);
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    let reopened = PrivateMetadataStore::open(&second, ids::ALICE, namespace)
        .await?
        .expect("the respawned store must still hold the PMS replica");
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

/// A start finds exactly the identities whose hosting was recorded, each
/// with its PMS's namespace and its replica store. Denied: an
/// identity provisioned and never recorded is not listed, and neither is
/// one whose record the disk refused — a read-only subdirectory, the
/// closest stand-in for a full disk at the commit point.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_start_finds_the_identities_whose_hosting_was_recorded() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let work = host_identity(&first, ids::ALICE_AT_WORK).await?;
    first
        .record_hosting(ids::ALICE_AT_WORK, work.namespace())
        .await?;
    let _never_recorded = host_identity(&first, ids::ALICE_AT_LEISURE).await?;

    let carol = host_identity(&first, ids::CAROL).await?;
    let carol_subdirectory = dir.path().join("identities").join(ids::CAROL.to_string());
    std::fs::set_permissions(&carol_subdirectory, std::fs::Permissions::from_mode(0o500))?;
    let refused = first.record_hosting(ids::CAROL, carol.namespace()).await;
    std::fs::set_permissions(&carol_subdirectory, std::fs::Permissions::from_mode(0o700))?;
    assert!(
        refused.is_err(),
        "a record the disk refuses must fail the commit"
    );
    first.shutdown().await?;
    drop(first);

    let second = node_on(dir.path()).await?;
    assert_eq!(
        second.recorded_hosting()?,
        vec![RecordedHosting {
            identity: ids::ALICE_AT_WORK,
            pms: work.namespace(),
            store_present: true,
        }],
        "a start must find the recorded identity and nothing else"
    );
    second.shutdown().await?;
    Ok(())
}

/// A pod's stores import from their tickets onto the replicas a respawned
/// node's store already holds, the import restart recovery performs, and the
/// pod is held again on the same stores.
#[tokio::test(flavor = "multi_thread")]
async fn a_pod_imports_onto_its_replicas_after_a_respawn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let family = PodId::from_bytes([
        0x9c, 0xbc, 0xbe, 0x4d, 0xa7, 0xcc, 0x35, 0xa4, 0x43, 0x60, 0xd6, 0x4e, 0x45, 0x62, 0x19,
        0x57,
    ]);

    let first = node_on(dir.path()).await?;
    let _pms = host_identity(&first, ids::ALICE).await?;
    first.create_pod(ids::ALICE, family).await?;
    let tickets = first
        .share_pod_tickets(ids::ALICE, family, AddrInfoOptions::Addresses)
        .await?;
    let stores = [
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    ];
    first.shutdown().await?;
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(ids::ALICE).await?;
    for namespace in stores {
        assert!(
            second.holds_replica(ids::ALICE, namespace).await?,
            "a pod's replica did not survive the respawn"
        );
    }
    second.import_pod(ids::ALICE, family, tickets).await?;
    let held = second
        .share_pod_tickets(ids::ALICE, family, AddrInfoOptions::Addresses)
        .await?;
    assert_eq!(
        [
            held.membership.capability.id(),
            held.records.capability.id()
        ],
        stores,
        "the respawned node holds the pod on other stores"
    );
    second.shutdown().await?;
    Ok(())
}

/// An operation on `note` by `writer` at its sequence 1, numbered as the
/// pods service numbers it: one above the highest its author holds there.
async fn append(on: &SyncNode, writer: PdnId, pod: PodId, note: &RecordRef) -> Result<()> {
    let author = on.default_author(writer)?;
    let view = on.pod_record_view_of(writer, pod, note).await?;
    let op_seq = view
        .next_op_seq(note, author)
        .ok_or_else(|| anyhow::anyhow!("exhausted"))?;
    let key = RecordKey::Operation {
        member: note.member,
        id: note.id,
        op: OpId {
            writer,
            author,
            mseq: Seq::FIRST,
            op_seq,
        },
    };
    on.write_pod_entry(writer, pod, PodStore::Records, &key.to_bytes(), b"op")
        .await
}

/// A device's operation sequence on a mergeable-document continues after a
/// respawn: its fourth operation takes sequence 4 under the same author.
/// The pod is imported from its tickets onto the replicas the store holds,
/// as restart recovery imports it.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_sequence_continues_after_a_respawn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = node_on(dir.path()).await?;
    let (bob, _pms) = c::host(&first).await?;
    let pod = c::create(&first, &bob).await?;
    let note = RecordRef {
        member: bob.id,
        kind: RecordKind::MergeableDocument,
        id: RecordId::from_bytes([0x11; 16]),
    };
    for _ in 0..3 {
        append(&first, bob.id, pod, &note).await?;
    }
    let author = first.default_author(bob.id)?;
    let tickets = c::tickets(&first, &bob, pod).await?;
    first.shutdown().await?;
    drop(first);

    let second = node_on(dir.path()).await?;
    second.provision_identity(bob.id).await?;
    second.import_pod(bob.id, pod, tickets).await?;
    append(&second, bob.id, pod, &note).await?;
    let numbered: Vec<(AuthorId, u64)> = second
        .read_pod_operations(bob.id, pod, &note)
        .await?
        .into_iter()
        .map(|op| (op.id.author, op.id.op_seq))
        .collect();
    assert_eq!(
        numbered,
        (1..=4).map(|op_seq| (author, op_seq)).collect::<Vec<_>>()
    );
    second.shutdown().await?;
    Ok(())
}

/// A node whose blob store cannot open fails its spawn with an error rather
/// than hanging (iroh-blobs-001). A directory in the place of the store's
/// database file is what fails the open.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_store_that_cannot_open_fails_the_spawn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("blobs").join("blobs.db"))?;
    let spawned = tokio::time::timeout(std::time::Duration::from_secs(30), node_on(dir.path()))
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the spawn hung on a blob store that cannot open"))?;
    let Err(err) = spawned else {
        anyhow::bail!("a node spawned on a blob store that cannot open");
    };
    assert!(
        format!("{err:#}").contains("cannot open the blob store"),
        "the spawn failed for another reason: {err:#}"
    );
    Ok(())
}

/// A membership store's entry is on disk with its payload when its write
/// returns: a copy of the directory taken then, as a kill leaves it, holds
/// the payload. The replica store is synced alone before the copy, the blob
/// store's batch still open: the order in which a kill would leave the entry
/// without the payload the membership view rests on.
#[tokio::test(flavor = "multi_thread")]
async fn a_membership_write_is_on_disk_whole_when_it_returns() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = node_on(dir.path()).await?;
    let (alice, _pms) = c::host(&node).await?;
    let pod = c::create(&node, &alice).await?;
    let payload = b"a statement no other device holds yet";
    node.write_pod_entry(alice.id, pod, PodStore::Membership, b"member/x", payload)
        .await?;
    node.sync_replica_store_for_test(alice.id).await?;
    let copy = tempfile::tempdir()?;
    test_utils::crash_copy(dir.path(), copy.path())?;
    node.shutdown().await?;
    drop(node);

    let reopened = node_on(copy.path()).await?;
    assert!(
        reopened
            .holds_payload(iroh_blobs::Hash::new(payload))
            .await?,
        "the membership entry's payload is not in the copy"
    );
    reopened.shutdown().await?;
    Ok(())
}
