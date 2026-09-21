//! One stack per hosted identity: a replica store of its own under its
//! own subdirectory, bounded at a cache share cut from the node's budget
//! at spawn, and a restart that puts both identities back on the
//! in-process path (ADR-0013).

use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::Result;
use data_layer::{
    claim_id_of, AddrInfoOptions, ConnectionMetadataStore, GrantedClaim, PrivateMetadataStore,
    ReadGrant, ShareMode, SpawnOptions, SyncNode, DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, host_identity, ids};

/// The in-process path has no gossip, so what crosses it crosses on a
/// pass; at this cadence a handful of passes is milliseconds.
const RECONCILE: Duration = Duration::from_millis(500);

const GRANTED: &str = "contact/email";

/// Where a hosted identity's replica store lands under `dir`.
fn store_of(dir: &std::path::Path, identity: PdnId) -> std::path::PathBuf {
    use std::fmt::Write as _;
    let hex = identity
        .as_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _written = write!(out, "{byte:02x}");
            out
        });
    dir.join("identities").join(hex).join("docs.redb")
}

/// Each hosted identity's replica store is a file of its own under a
/// subdirectory named for that identity, and an identity the node does
/// not host has neither.
#[tokio::test(flavor = "multi_thread")]
async fn each_identity_opens_its_replica_store_under_its_own_subdirectory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = SyncNode::spawn(SpawnOptions::on_directory(dir.path())).await?;
    node.provision_identity(ids::ALICE_AT_WORK).await?;
    node.provision_identity(ids::ALICE_AT_LEISURE).await?;

    let work = store_of(dir.path(), ids::ALICE_AT_WORK);
    let leisure = store_of(dir.path(), ids::ALICE_AT_LEISURE);
    assert_ne!(work, leisure);
    for (identity, store) in [
        (ids::ALICE_AT_WORK, &work),
        (ids::ALICE_AT_LEISURE, &leisure),
    ] {
        assert!(
            store.is_file(),
            "{identity} has no replica store of its own at {}",
            store.display()
        );
    }
    assert!(
        !store_of(dir.path(), ids::CAROL).exists(),
        "an identity the node does not host was given a subdirectory"
    );

    node.shutdown().await?;
    Ok(())
}

/// The bound every replica store of a node opens at is the node's budget
/// divided by the identities the device is provisioned for: the whole
/// budget when neither value is named, the cut share when both are, and
/// a node holding more identities than it was provisioned for says so.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_share_is_the_budget_cut_by_the_provisioned_count() -> Result<()> {
    let plain = SyncNode::spawn(SpawnOptions::memory()).await?;
    assert_eq!(
        plain.replica_cache_share_bytes(),
        DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
        "a node that names neither value must give its one identity the whole budget"
    );
    plain.provision_identity(ids::ALICE).await?;
    assert!(
        !plain.replica_cache_budget_exceeded()?,
        "one identity on a node provisioned for one is within the budget"
    );

    let budget = 4 * 1024 * 1024;
    let provisioned = 4;
    let cut = SyncNode::spawn(SpawnOptions {
        replica_cache_budget_bytes: budget,
        provisioned_identities: NonZeroUsize::new(provisioned).expect("four"),
        ..SpawnOptions::memory()
    })
    .await?;
    assert_eq!(cut.replica_cache_share_bytes(), budget / provisioned);

    // A node that outgrows the count its share was cut from reports it,
    // and not before.
    for identity in [ids::ALICE, ids::BOB, ids::CAROL, ids::DAVE] {
        cut.provision_identity(identity).await?;
    }
    assert!(
        !cut.replica_cache_budget_exceeded()?,
        "a node holding exactly what it is provisioned for is within the budget"
    );
    cut.provision_identity(ids::ALICE_AT_WORK).await?;
    assert!(
        cut.replica_cache_budget_exceeded()?,
        "a node holding more identities than its share was cut for must report it"
    );

    plain.shutdown().await?;
    cut.shutdown().await?;
    Ok(())
}

/// An identity brought up on a running node opens its store at the share
/// the node cut at spawn, and the identity already hosted keeps running:
/// its store is not reopened, so the handles held across the act still
/// read and write.
#[tokio::test(flavor = "multi_thread")]
async fn an_identity_created_on_a_running_node_opens_at_the_same_share() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let budget = 8 * 1024 * 1024;
    let node = SyncNode::spawn(SpawnOptions {
        replica_cache_budget_bytes: budget,
        provisioned_identities: NonZeroUsize::new(2).expect("two"),
        ..SpawnOptions::on_directory(dir.path())
    })
    .await?;
    let share = node.replica_cache_share_bytes();
    assert_eq!(share, budget / 2);

    let work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let path = EntryPath::new(GRANTED)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &path,
        b"alice@work.example",
    )
    .await?;

    node.provision_identity(ids::ALICE_AT_LEISURE).await?;
    assert_eq!(
        node.replica_cache_share_bytes(),
        share,
        "an identity created while the node runs must open at the share cut at spawn"
    );
    assert!(
        store_of(dir.path(), ids::ALICE_AT_LEISURE).is_file(),
        "the identity created while the node runs has no store"
    );

    // The store the node already had open was not reopened: the handles
    // from before still answer, and its author still writes.
    assert!(
        work.list_devices().await?.contains(&node.node_id()),
        "the directory held across the act stopped answering"
    );
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &path,
        b"alice2@work.example",
    )
    .await?;
    assert_eq!(
        node.read(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, &path)
            .await?
            .as_deref(),
        Some(b"alice2@work.example".as_ref()),
    );

    node.shutdown().await?;
    Ok(())
}

/// Two identities of one node that converged in the process converge
/// again after a restart: every address they hold of each other was
/// minted by this node before it stopped — the tickets in each
/// identity's directory, the device records in them — and each such dial
/// takes the in-process path rather than failing against the node's own
/// endpoint.
///
/// Denied: nothing the grant does not cover crosses the restart either,
/// so the replica that comes back is the scoped one and not the issuer's
/// whole store.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario: the run before the restart and the run after
async fn two_identities_of_one_node_converge_again_after_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let granted = EntryPath::new(GRANTED)?;
    let withheld = EntryPath::new("notes/diary")?;

    let first = spawn_on(dir.path()).await?;
    let work_directory = host_identity(&first, ids::ALICE_AT_WORK).await?;
    let leisure_directory = host_identity(&first, ids::ALICE_AT_LEISURE).await?;
    let work_namespace = work_directory.namespace();
    let leisure_namespace = leisure_directory.namespace();

    first
        .create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let work_author = first.default_author(ids::ALICE_AT_WORK)?;
    first
        .write(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            work_author,
            &granted,
            b"alice@work.example",
        )
        .await?;
    first
        .write(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            work_author,
            &withheld,
            b"dear diary",
        )
        .await?;

    // The connection pair, and the grant with the ticket it carries.
    let work_own = ConnectionMetadataStore::create(&first, ids::ALICE_AT_WORK).await?;
    work_own.publish_device(first.node_id()).await?;
    let leisure_own = ConnectionMetadataStore::create(&first, ids::ALICE_AT_LEISURE).await?;
    leisure_own.publish_device(first.node_id()).await?;
    let work_read = work_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::Addresses)
        .await?;
    let leisure_read = leisure_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::Addresses)
        .await?;
    let leisure_as_work_sees =
        ConnectionMetadataStore::import(&first, ids::ALICE_AT_WORK, leisure_read.clone()).await?;
    let work_as_leisure_sees =
        ConnectionMetadataStore::import(&first, ids::ALICE_AT_LEISURE, work_read.clone()).await?;
    first.host_connection(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_LEISURE,
        &work_own,
        &leisure_as_work_sees,
    )?;
    first.host_connection(
        ids::ALICE_AT_LEISURE,
        ids::ALICE_AT_WORK,
        &leisure_own,
        &work_as_leisure_sees,
    )?;

    let data_read = first
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::Addresses,
        )
        .await?;
    let data_write = first
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Write,
            AddrInfoOptions::Addresses,
        )
        .await?;
    work_own
        .publish_grant(
            &ReadGrant {
                issuer: ids::ALICE_AT_WORK,
                audience: ids::ALICE_AT_LEISURE,
                claims: NonEmpty::new(GrantedClaim {
                    claim: claim_id_of(&ids::ALICE_AT_WORK, &granted),
                    write: false,
                }),
            },
            &data_read,
        )
        .await?;
    first
        .import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, data_read.clone())
        .await?;
    assert!(
        eventually(|| async {
            Ok(first
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted)
                .await?
                .as_deref()
                == Some(b"alice@work.example".as_ref()))
        })
        .await?,
        "the granted claim did not cross before the restart"
    );

    // Everything the restart needs, where recovery looks for it.
    work_directory.put_ticket("connection", &work_read).await?;
    work_directory.put_ticket("data", &data_write).await?;
    leisure_directory
        .put_ticket("connection", &leisure_read)
        .await?;
    leisure_directory.put_ticket("granted", &data_read).await?;
    first
        .flush_replicas(ids::ALICE_AT_WORK, work_namespace)
        .await?;
    first
        .flush_replicas(ids::ALICE_AT_LEISURE, leisure_namespace)
        .await?;
    first.shutdown().await?;
    drop(work_own);
    drop(leisure_own);
    drop(work_directory);
    drop(leisure_directory);
    drop(first);

    // The restart. Every address below was minted by this node before it
    // stopped, so every dial it produces names this node's own endpoint.
    let second = spawn_on(dir.path()).await?;
    second.provision_identity(ids::ALICE_AT_WORK).await?;
    second.provision_identity(ids::ALICE_AT_LEISURE).await?;
    let work_directory = PrivateMetadataStore::open(&second, ids::ALICE_AT_WORK, work_namespace)
        .await?
        .expect("the work directory must come back");
    let leisure_directory =
        PrivateMetadataStore::open(&second, ids::ALICE_AT_LEISURE, leisure_namespace)
            .await?
            .expect("the leisure directory must come back");
    second.host_identity(ids::ALICE_AT_WORK, &work_directory)?;
    second.host_identity(ids::ALICE_AT_LEISURE, &leisure_directory)?;

    let work_own = ConnectionMetadataStore::import(
        &second,
        ids::ALICE_AT_WORK,
        work_directory
            .get_ticket("connection")
            .await?
            .expect("work's own connection ticket"),
    )
    .await?;
    let leisure_own = ConnectionMetadataStore::import(
        &second,
        ids::ALICE_AT_LEISURE,
        leisure_directory
            .get_ticket("connection")
            .await?
            .expect("leisure's own connection ticket"),
    )
    .await?;
    let leisure_as_work_sees =
        ConnectionMetadataStore::import(&second, ids::ALICE_AT_WORK, leisure_read).await?;
    let work_as_leisure_sees =
        ConnectionMetadataStore::import(&second, ids::ALICE_AT_LEISURE, work_read).await?;
    second.host_connection(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_LEISURE,
        &work_own,
        &leisure_as_work_sees,
    )?;
    second.host_connection(
        ids::ALICE_AT_LEISURE,
        ids::ALICE_AT_WORK,
        &leisure_own,
        &work_as_leisure_sees,
    )?;
    second
        .import_namespace(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            work_directory
                .get_ticket("data")
                .await?
                .expect("work's data ticket"),
        )
        .await?;
    second
        .import_namespace_scoped(
            ids::ALICE_AT_LEISURE,
            ids::ALICE_AT_WORK,
            leisure_directory
                .get_ticket("granted")
                .await?
                .expect("leisure's granted ticket"),
        )
        .await?;

    // A write after the restart crosses, so the dials the restored
    // contacts produce ran in the process.
    let work_author = second.default_author(ids::ALICE_AT_WORK)?;
    second
        .write(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            work_author,
            &granted,
            b"alice-restarted@work.example",
        )
        .await?;
    assert!(
        eventually(|| async {
            Ok(second
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted)
                .await?
                .as_deref()
                == Some(b"alice-restarted@work.example".as_ref()))
        })
        .await?,
        "a write did not cross between two identities of one node after a restart"
    );
    assert!(
        second.in_process_sessions(ids::ALICE_AT_LEISURE)? > 0,
        "the restored contacts dialed the node's own endpoint instead of taking the \
         in-process path"
    );

    // Denied: the restart restores what the grant covers and no more.
    assert_eq!(
        second
            .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &withheld)
            .await?,
        None,
        "the withheld claim crossed the restart"
    );

    second.shutdown().await?;
    Ok(())
}

async fn spawn_on(dir: &std::path::Path) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::on_directory(dir)
    })
    .await
}

/// Every create and import names the identity whose stores hold it, and
/// one naming an identity this node does not host is refused with
/// nothing registered — not for the named identity, which has no stack,
/// and not for the identity hosted beside it.
///
/// Denied: the directory, the connection metadata store and the data
/// namespace all refuse the same way, and the hosted identity's own
/// issuer stays the only one it resolves.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_naming_an_identity_the_node_does_not_host_registers_nothing() -> Result<()> {
    let node = SyncNode::spawn(SpawnOptions::memory()).await?;
    let work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let path = EntryPath::new(GRANTED)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &path,
        b"alice@work.example",
    )
    .await?;
    let data_ticket = node
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    let directory_ticket = work
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;

    for refusal in [
        PrivateMetadataStore::create(&node, ids::ALICE_AT_LEISURE)
            .await
            .err(),
        PrivateMetadataStore::import(&node, ids::ALICE_AT_LEISURE, directory_ticket)
            .await
            .err(),
        ConnectionMetadataStore::create(&node, ids::ALICE_AT_LEISURE)
            .await
            .err(),
        node.import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, data_ticket)
            .await
            .err(),
    ] {
        let refusal = refusal.expect("an act naming an unhosted identity must be refused");
        assert!(
            refusal
                .downcast_ref::<data_layer::IdentityNotProvisioned>()
                .is_some(),
            "the refusal did not name the unprovisioned identity: {refusal:#}"
        );
    }

    // Nothing was registered for either identity: the named one still has
    // no stack, and the hosted one resolves only the issuer it created.
    let unhosted = node
        .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &path)
        .await
        .expect_err("an unhosted identity must resolve nothing");
    assert!(unhosted
        .downcast_ref::<data_layer::IdentityNotProvisioned>()
        .is_some());
    let unknown = node
        .read(ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE, &path)
        .await
        .expect_err("the hosted identity must not have gained an issuer");
    assert!(unknown
        .downcast_ref::<data_layer::UnknownIssuer>()
        .is_some());
    assert_eq!(
        node.read(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK, &path)
            .await?
            .as_deref(),
        Some(b"alice@work.example".as_ref()),
        "the hosted identity's own replica must be untouched"
    );

    node.shutdown().await?;
    Ok(())
}
