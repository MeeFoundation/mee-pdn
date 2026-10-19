//! One stack per hosted identity: a replica store of its own under its
//! own subdirectory, bounded at a share of the node's cache budget cut
//! as that store opens, and a restart that puts both identities back on
//! the in-process path (ADR-0013).

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    pod_ticket_kind, AddrInfoOptions, PodStore, PodTickets, PrivateMetadataStore, Seq,
    SpawnOptions, SyncNode, UnknownPod, DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
};
use pdn_types::{PdnId, PodId};
use test_utils::{host_identity, ids, pod as c};

/// The in-process path has no gossip, so what crosses it crosses on a
/// pass; at this cadence a handful of passes is milliseconds.
const RECONCILE: Duration = Duration::from_millis(500);

const FAMILY: PodId = PodId::from_bytes([0xfa; 16]);

/// Both of `pod`'s tickets in `pms`, and its record there.
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

/// Both of `pod`'s tickets, as [`publish_pod`] left them in `pms`.
async fn published_pod(pms: &PrivateMetadataStore, pod: PodId) -> Result<PodTickets> {
    let ticket = async |store| {
        pms.get_ticket(&pod_ticket_kind(&pod, store))
            .await?
            .ok_or_else(|| anyhow::anyhow!("a pod's ticket and its payload are local"))
    };
    Ok(PodTickets {
        membership: ticket(PodStore::Membership).await?,
        records: ticket(PodStore::Records).await?,
    })
}

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

/// A replica store opens at the node's budget divided by the identities
/// the storage directory records as hosted, its own included: the whole
/// budget for a device carrying one, half each once two are recorded, and
/// a store in memory carries no bound at all. A second identity
/// provisioned while the node runs leaves the first's bound where it is,
/// so the bounds handed out pass the budget until the next start cuts them
/// from the whole set. Denied: a store set no commit recorded — what an
/// unfinished create leaves — takes no share at the next start.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, memory, a plain directory and a budgeted one in one place
async fn a_store_opens_at_its_share_of_the_node_budget() -> Result<()> {
    let memory = SyncNode::spawn(SpawnOptions::memory()).await?;
    memory.provision_identity(ids::ALICE).await?;
    assert_eq!(
        memory.replica_cache_share_bytes(ids::ALICE)?,
        None,
        "a store in memory must carry no cache bound"
    );
    assert!(
        !memory.replica_cache_budget_exceeded()?,
        "stores that carry no bound cannot pass the budget"
    );
    memory.shutdown().await?;

    let plain_dir = tempfile::tempdir()?;
    let plain = SyncNode::spawn(SpawnOptions::on_directory(plain_dir.path())).await?;
    plain.provision_identity(ids::ALICE).await?;
    assert_eq!(
        plain.replica_cache_share_bytes(ids::ALICE)?,
        Some(DEFAULT_REPLICA_CACHE_BUDGET_BYTES),
        "a node that names no budget must give its one identity the whole default"
    );
    plain.shutdown().await?;

    let dir = tempfile::tempdir()?;
    let budget = 8 * 1024 * 1024;
    let node = SyncNode::spawn(SpawnOptions {
        replica_cache_budget_bytes: budget,
        ..SpawnOptions::on_directory(dir.path())
    })
    .await?;
    let work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    node.record_hosting(ids::ALICE_AT_WORK, work.namespace())
        .await?;
    assert_eq!(
        node.replica_cache_share_bytes(ids::ALICE_AT_WORK)?,
        Some(budget),
        "the one identity a directory holds must take the whole budget"
    );
    assert!(
        !node.replica_cache_budget_exceeded()?,
        "one store at the whole budget is within it"
    );

    let leisure = host_identity(&node, ids::ALICE_AT_LEISURE).await?;
    node.record_hosting(ids::ALICE_AT_LEISURE, leisure.namespace())
        .await?;
    assert_eq!(
        node.replica_cache_share_bytes(ids::ALICE_AT_LEISURE)?,
        Some(budget / 2),
        "the second identity of a directory must take half the budget"
    );
    assert_eq!(
        node.replica_cache_share_bytes(ids::ALICE_AT_WORK)?,
        Some(budget),
        "the bound of an open store cannot change, so the first identity keeps what it opened at"
    );
    assert!(
        node.replica_cache_budget_exceeded()?,
        "a node grown while it ran must report that the bounds handed out pass its budget"
    );
    // What a create that never reached its commit point leaves on disk.
    node.provision_identity(ids::CAROL).await?;
    node.unhost_identity(ids::CAROL).await?;
    node.shutdown().await?;
    drop(work);
    drop(leisure);

    let again = SyncNode::spawn(SpawnOptions {
        replica_cache_budget_bytes: budget,
        ..SpawnOptions::on_directory(dir.path())
    })
    .await?;
    for identity in [ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE] {
        again.provision_identity(identity).await?;
        assert_eq!(
            again.replica_cache_share_bytes(identity)?,
            Some(budget / 2),
            "a start must cut every share from the identities the directory records, and no more"
        );
    }
    assert!(
        !again.replica_cache_budget_exceeded()?,
        "two stores at half the budget each are within it"
    );
    again.shutdown().await?;
    Ok(())
}

/// An identity brought up on a running node leaves the identity already
/// hosted running: its store is not reopened for the newcomer's share,
/// so the handles held across the act still read and write.
#[tokio::test(flavor = "multi_thread")]
async fn an_identity_created_on_a_running_node_leaves_the_open_store_alone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let budget = 8 * 1024 * 1024;
    let node = SyncNode::spawn(SpawnOptions {
        replica_cache_budget_bytes: budget,
        ..SpawnOptions::on_directory(dir.path())
    })
    .await?;

    let work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    work.record_pod(FAMILY, Seq::FIRST).await?;

    let share = node.replica_cache_share_bytes(ids::ALICE_AT_WORK)?;
    node.provision_identity(ids::ALICE_AT_LEISURE).await?;
    assert_eq!(
        node.replica_cache_share_bytes(ids::ALICE_AT_WORK)?,
        share,
        "the store open across the act must keep the bound it opened at"
    );
    assert!(
        store_of(dir.path(), ids::ALICE_AT_LEISURE).is_file(),
        "the identity created while the node runs has no store"
    );

    // The store the node already had open was not reopened: the handles
    // from before still answer, and its author still writes.
    assert!(
        work.list_devices().await?.contains(&node.node_id()),
        "the PMS held across the act stopped answering"
    );
    work.tombstone_pod(FAMILY, Seq::new(2)).await?;
    assert_eq!(work.pod_record(FAMILY).await?, Some((Seq::new(2), false)));

    node.shutdown().await?;
    Ok(())
}

/// Two members of one pod hosted on one node that converged in the
/// process converge again after a restart: every address they hold of each
/// other was minted by this node before it stopped — the pod's tickets in
/// each identity's PMS, the device statements in its membership store —
/// and each such dial takes the in-process path rather than failing
/// against the node's own endpoint.
///
/// Denied: a co-located identity holding both tickets that is no member
/// takes nothing across the restart either.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_of_one_node_converge_again_after_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let first = spawn_on(dir.path()).await?;
    let (work, work_pms) = c::host(&first).await?;
    let (leisure, leisure_pms) = c::host(&first).await?;
    let (carol, carol_pms) = c::host(&first).await?;
    let pod = c::create(&first, &work).await?;
    c::invite(
        &first,
        &work,
        pod,
        &leisure,
        vec![c::device_of(&first, &leisure)?],
    )
    .await?;
    let tickets = c::tickets(&first, &work, pod).await?;
    first.import_pod(leisure.id, pod, tickets.clone()).await?;
    first.import_pod(carol.id, pod, tickets.clone()).await?;
    let before = c::place_claim(&first, &work, pod, 0x01).await?;
    assert!(
        c::reads(&first, leisure.id, pod, before).await?,
        "the claim did not cross before the restart"
    );

    // Everything the restart needs, where recovery looks for it.
    let mut hosted = Vec::new();
    for (identity, pms) in [
        (work.id, &work_pms),
        (leisure.id, &leisure_pms),
        (carol.id, &carol_pms),
    ] {
        publish_pod(pms, pod, &tickets).await?;
        first.flush_stores(identity).await?;
        hosted.push((identity, pms.namespace()));
    }
    first.shutdown().await?;
    drop((work_pms, leisure_pms, carol_pms));
    drop(first);

    // The restart. Every address below was minted by this node before it
    // stopped, so every dial it produces names this node's own endpoint.
    let second = spawn_on(dir.path()).await?;
    for (identity, namespace) in hosted {
        second.provision_identity(identity).await?;
        let pms = PrivateMetadataStore::open(&second, identity, namespace)
            .await?
            .expect("a PMS must come back");
        second.host_identity(identity, &pms)?;
        second
            .import_pod(identity, pod, published_pod(&pms, pod).await?)
            .await?;
    }

    let after = c::place_claim(&second, &work, pod, 0x02).await?;
    assert!(
        c::reads(&second, leisure.id, pod, after).await?,
        "a record did not cross between two identities of one node after a restart"
    );
    assert!(
        second.in_process_sessions(leisure.id)? > 0,
        "the restored contacts dialed the node's own endpoint instead of taking the \
         in-process path"
    );

    // Denied: the restart restores nothing to an identity that is no member.
    assert_eq!(
        c::held_records(&second, carol.id, pod).await?,
        Vec::<String>::new(),
        "the co-located ticket holder took records across the restart"
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
/// one naming an identity this node does not host is refused with nothing
/// registered — not for the named identity, which has no stack, and not
/// for the identity hosted beside it.
///
/// Denied: the PMS and a pod's stores refuse the same way, and the hosted
/// identity's own pod stays the only one it holds.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_naming_an_identity_the_node_does_not_host_registers_nothing() -> Result<()> {
    let node = SyncNode::spawn(SpawnOptions::memory()).await?;
    let (work, work_pms) = c::host(&node).await?;
    let pod = c::create(&node, &work).await?;
    let record = c::place_claim(&node, &work, pod, 0x01).await?;
    let tickets = c::tickets(&node, &work, pod).await?;
    let pms_ticket = work_pms
        .share_ticket(AddrInfoOptions::RelayAndAddresses)
        .await?;

    for refusal in [
        PrivateMetadataStore::create(&node, ids::ALICE_AT_LEISURE)
            .await
            .err(),
        PrivateMetadataStore::import(&node, ids::ALICE_AT_LEISURE, pms_ticket)
            .await
            .err(),
        node.create_pod(ids::ALICE_AT_LEISURE, FAMILY).await.err(),
        node.import_pod(ids::ALICE_AT_LEISURE, pod, tickets)
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
    // no stack, and the hosted one holds only the pod it created.
    let unhosted = node
        .read_pod_record(ids::ALICE_AT_LEISURE, pod, &record)
        .await
        .expect_err("an unhosted identity must hold nothing");
    assert!(unhosted
        .downcast_ref::<data_layer::IdentityNotProvisioned>()
        .is_some());
    let unknown = node
        .pod_stores(work.id, FAMILY)
        .expect_err("the hosted identity must not have gained a pod");
    assert!(unknown.downcast_ref::<UnknownPod>().is_some());
    assert_eq!(
        node.read_pod_record(work.id, pod, &record)
            .await?
            .as_deref(),
        Some(b"claim".as_ref()),
        "the hosted identity's own pod must be untouched"
    );

    node.shutdown().await?;
    Ok(())
}
