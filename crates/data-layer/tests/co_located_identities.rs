//! Two identities of one node meet inside the process: iroh refuses a
//! connection to the endpoint's own id, so what they exchange runs over a
//! pipe under the same protocol, the same classification and the same
//! egress filter as a session between two nodes (ADR-0013).
//!
//! A pod's join lives in pdn-node; here its acts travel by the store-level
//! writes the join dialogue performs.

use std::time::Duration;

use anyhow::Result;
use data_layer::{NamespaceId, RecordKey, Seq, SpawnOptions, SyncNode, UnknownPod};
use pdn_types::{PodId, RecordId};
use test_utils::{
    eventually,
    pod::{self as c, Person},
};

/// A scoped identity has no gossip path, so a negative assertion is "the
/// reader retried over several passes and was refused"; at this cadence
/// that is milliseconds rather than the production default's tens of
/// seconds.
const RECONCILE: Duration = Duration::from_millis(500);

async fn spawn_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// A pod pass out of reach: what crosses between co-located members then
/// crosses on an announcement or on the periodic pass alone.
const QUIET: Duration = Duration::from_secs(3_600);

async fn pod_node(
    reconcile_interval: Duration,
    pod_reconcile_interval: Duration,
) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval,
        pod_reconcile_interval,
        ..SpawnOptions::memory()
    })
    .await
}

/// `creator`'s pod on `node` with every one of `members` invited from
/// `node` and every one of `members` and `holders` holding its stores;
/// returns the pod and the namespace of its record store.
async fn shared_pod(
    node: &SyncNode,
    creator: &Person,
    members: &[&Person],
    holders: &[&Person],
) -> Result<(PodId, NamespaceId)> {
    let pod = c::create(node, creator).await?;
    for member in members {
        c::invite(
            node,
            creator,
            pod,
            member,
            vec![c::device_of(node, member)?],
        )
        .await?;
    }
    let tickets = c::tickets(node, creator, pod).await?;
    let records = tickets.records.capability.id();
    for holder in members.iter().chain(holders) {
        node.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    Ok((pod, records))
}

/// A record one member of a pod places reaches a co-located member with no
/// other node reachable, payload included.
///
/// Denied: a third identity of the same node holding both of the pod's
/// tickets and no membership obtains nothing — not from the announcement of
/// either write, and not over several passes. A second record crossing
/// orders the absences.
#[tokio::test(flavor = "multi_thread")]
async fn a_co_located_member_converges_and_a_co_located_ticket_holder_takes_nothing() -> Result<()>
{
    let node = pod_node(RECONCILE, RECONCILE).await?;
    let (work, _) = c::host(&node).await?;
    let (leisure, _) = c::host(&node).await?;
    let (carol, _) = c::host(&node).await?;
    let (pod, _records) = shared_pod(&node, &work, &[&leisure], &[&carol]).await?;

    let first = c::place_claim(&node, &work, pod, 0x01).await?;
    assert!(
        c::reads(&node, leisure.id, pod, first).await?,
        "the record did not reach the co-located member"
    );
    assert_eq!(
        node.read_pod_record(leisure.id, pod, &first)
            .await?
            .as_deref(),
        Some(b"claim".as_ref())
    );

    // Sentinel: a second record that has crossed proves a further wave ran.
    let second = c::place_claim(&node, &work, pod, 0x02).await?;
    assert!(
        c::reads(&node, leisure.id, pod, second).await?,
        "the second wave did not cross"
    );
    // Denied (the announcement to a ticket holder that is no member).
    assert_eq!(
        c::held_records(&node, carol.id, pod).await?,
        Vec::<String>::new(),
        "the announcement carried entries to a co-located ticket holder"
    );

    // Denied (passes).
    let passes = node.reconcile_passes();
    assert!(
        eventually(|| async { Ok(node.reconcile_passes() >= passes + 3) }).await?,
        "the node ran no passes"
    );
    assert_eq!(
        c::held_records(&node, carol.id, pod).await?,
        Vec::<String>::new(),
        "a co-located identity holding the tickets and no membership obtained entries"
    );
    assert!(
        c::lists_nobody(&node, carol.id, pod).await?,
        "a co-located ticket holder took membership entries"
    );

    node.shutdown().await?;
    Ok(())
}

/// An identity of a node reads nothing of a pod only a co-located identity
/// holds, and is answered as it is for a pod no identity here holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_pod_a_co_located_identity_holds_is_unknown() -> Result<()> {
    let node = spawn_node().await?;
    let (work, _) = c::host(&node).await?;
    let (leisure, _) = c::host(&node).await?;
    let pod = c::create(&node, &work).await?;
    let record = c::place_claim(&node, &work, pod, 0x01).await?;

    let read = node
        .read_pod_record(leisure.id, pod, &record)
        .await
        .expect_err("a co-located identity read a pod it does not hold");
    assert!(
        read.downcast_ref::<UnknownPod>().is_some(),
        "the refusal did not read as an unknown pod: {read:#}"
    );
    let listed = node
        .pod_record_view(leisure.id, pod)
        .await
        .expect_err("a co-located identity listed a pod it does not hold");
    assert!(
        listed.downcast_ref::<UnknownPod>().is_some(),
        "the refusal did not read as an unknown pod: {listed:#}"
    );

    // The same answer for a pod no identity here holds at all.
    let absent = node
        .read_pod_record(leisure.id, PodId::from_bytes([0xfa; 16]), &record)
        .await
        .expect_err("a pod nobody holds was read");
    assert!(
        absent.downcast_ref::<UnknownPod>().is_some(),
        "an unheld pod answered differently: {absent:#}"
    );

    node.shutdown().await?;
    Ok(())
}

/// Two members of one pod on one node each write with their own author, so
/// an entry says which identity wrote it.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_of_one_node_write_as_two_authors() -> Result<()> {
    let node = spawn_node().await?;
    let (work, _) = c::host(&node).await?;
    let (leisure, _) = c::host(&node).await?;
    let (pod, _records) = shared_pod(&node, &work, &[&leisure], &[]).await?;

    let claim_of = |member: &Person, seed: u8| RecordKey::Claim {
        member: member.id,
        id: RecordId::from_bytes([seed; 16]),
        mseq: Seq::FIRST,
    };
    c::place_claim(&node, &work, pod, 0x01).await?;
    c::place_claim(&node, &leisure, pod, 0x02).await?;
    assert!(
        c::reads(&node, leisure.id, pod, claim_of(&work, 0x01).record()).await?,
        "the work identity's record did not cross"
    );

    let view = node.pod_record_view(leisure.id, pod).await?;
    let author_at = |key: RecordKey| {
        let key = key.to_bytes();
        view.verdicts()
            .find(|(entry, _verdict)| entry.key == key)
            .map(|(entry, _verdict)| entry.author)
    };
    let work_author = node.default_author(work.id)?;
    let leisure_author = node.default_author(leisure.id)?;
    assert_ne!(
        work_author, leisure_author,
        "two identities of one node write under one author"
    );
    assert_eq!(author_at(claim_of(&work, 0x01)), Some(work_author));
    assert_eq!(author_at(claim_of(&leisure, 0x02)), Some(leisure_author));

    node.shutdown().await?;
    Ok(())
}

/// Whether `node`'s pass over the co-located pairs comes to open no session
/// on `namespace` across two of its intervals.
async fn pass_goes_quiet(node: &SyncNode, namespace: NamespaceId) -> Result<bool> {
    eventually(|| async {
        let before = node.co_located_pass_sessions_of(namespace);
        tokio::time::sleep(RECONCILE * 2).await;
        Ok(node.co_located_pass_sessions_of(namespace) == before)
    })
    .await
}

/// A record that reaches one co-located member from another node reaches
/// the other co-located member on the periodic pass, and a pod that has
/// gone quiet is left alone: the other member is out of the pod's swarms
/// and its pod pass out of reach, and an entry arriving by sync announces
/// to no co-located identity, so what delivers can only be the pass; once
/// delivered, the pass opens no further session over several intervals.
#[tokio::test(flavor = "multi_thread")]
async fn the_periodic_pass_catches_up_a_co_located_pair_and_leaves_a_quiet_one_alone() -> Result<()>
{
    let phone = pod_node(QUIET, QUIET).await?;
    let tablet = pod_node(RECONCILE, QUIET).await?;
    let (erin, _) = c::host(&phone).await?;
    let (work, _) = c::host(&tablet).await?;
    let (leisure, _) = c::host(&tablet).await?;
    let pod = c::create(&tablet, &work).await?;
    c::invite(
        &tablet,
        &work,
        pod,
        &leisure,
        vec![c::device_of(&tablet, &leisure)?],
    )
    .await?;
    c::invite(
        &tablet,
        &work,
        pod,
        &erin,
        vec![c::device_of(&phone, &erin)?],
    )
    .await?;
    let tickets = c::tickets(&tablet, &work, pod).await?;
    let (membership, records) = (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    );
    tablet.import_pod(leisure.id, pod, tickets.clone()).await?;
    phone.import_pod(erin.id, pod, tickets).await?;
    for (node, holder) in [(&tablet, &leisure), (&phone, &erin)] {
        assert!(
            c::lists_device(node, holder.id, pod, erin.id, c::device_of(&phone, &erin)?).await?,
            "the pod did not converge before the act"
        );
    }
    for namespace in [membership, records] {
        tablet.leave_swarm_for_test(leisure.id, namespace).await?;
    }

    // Counted from before the claim, once the pass has gone quiet: the pass
    // can carry the claim across the moment it reaches the member in the
    // swarm, ahead of any read here.
    assert!(
        pass_goes_quiet(&tablet, records).await?,
        "the pass never went quiet before the claim"
    );
    let healed = tablet.co_located_pass_sessions_of(records);
    let claim = c::place_claim(&phone, &erin, pod, 0x01).await?;
    assert!(
        c::reads(&tablet, work.id, pod, claim).await?,
        "the claim did not reach the member in the swarm"
    );
    assert!(
        c::reads(&tablet, leisure.id, pod, claim).await?,
        "the periodic pass did not catch the co-located pair up"
    );
    assert!(
        tablet.co_located_pass_sessions_of(records) > healed,
        "the claim crossed with no pass session to carry it"
    );

    // The pass judges the pod by what has moved since it last reconciled
    // it, so a delivery costs one more pass: the one that observes that
    // nothing moved after it. Wait for that to settle, then hold the pod
    // to silence.
    assert!(
        pass_goes_quiet(&tablet, records).await?,
        "the pass never stopped reconciling the pair"
    );
    let before = tablet.co_located_pass_sessions_of(records);
    tokio::time::sleep(RECONCILE * 6).await;
    assert_eq!(
        tablet.co_located_pass_sessions_of(records),
        before,
        "a pass over a quiet co-located pod opened a session"
    );

    for node in [phone, tablet] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A record placed in a pod reaches a co-located member within one
/// reconcile interval, which is the announcement's doing: the record is
/// placed right after a pass has demonstrably run, so the next one is a
/// whole interval away and what delivers inside the bound below cannot be
/// a pass.
///
/// Denied: a co-located identity holding the pod's tickets that is no
/// member takes nothing from the same announcement, which names a namespace
/// and an identity and never an entry.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_reaches_a_co_located_identity_before_the_next_pass() -> Result<()> {
    // Long enough that the bound below sits well inside it, short enough
    // that the arrangement converges over a handful of passes.
    const INTERVAL: Duration = Duration::from_secs(3);
    /// What an announcement takes: a session over a pipe, microseconds.
    /// A third of the interval leaves the slop of observing a pass.
    const ANNOUNCED_WITHIN: Duration = Duration::from_secs(1);
    /// A pass opens one session per pod in a burst; this is how long with
    /// none opened says the burst is over.
    const PASS_SETTLED: Duration = Duration::from_millis(300);

    let node = pod_node(INTERVAL, QUIET).await?;
    let (work, _) = c::host(&node).await?;
    let (leisure, _) = c::host(&node).await?;
    let (erin, _) = c::host(&node).await?;
    let (pod, _records) = shared_pod(&node, &work, &[&leisure], &[&erin]).await?;
    let first = c::place_claim(&node, &work, pod, 0x01).await?;
    assert!(
        c::reads(&node, leisure.id, pod, first).await?,
        "the arrangement never converged — the premise of this test"
    );

    // The act, right after a pass: the quiet that follows one burst puts
    // the next a whole interval away.
    assert!(
        eventually(|| async {
            let before = node.in_process_sessions(leisure.id)?;
            tokio::time::sleep(PASS_SETTLED).await;
            Ok(node.in_process_sessions(leisure.id)? == before)
        })
        .await?,
        "the member's sessions never quieted, so no interval starts here"
    );
    let second = c::place_claim(&node, &work, pod, 0x02).await?;
    let deadline = tokio::time::Instant::now() + ANNOUNCED_WITHIN;
    let mut announced = false;
    while tokio::time::Instant::now() < deadline {
        if node
            .read_pod_record(leisure.id, pod, &second)
            .await?
            .is_some()
        {
            announced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        announced,
        "the record did not reach the co-located member inside the interval"
    );
    assert_eq!(
        c::held_records(&node, erin.id, pod).await?,
        Vec::<String>::new(),
        "the announcement carried entries to a co-located ticket holder"
    );

    node.shutdown().await?;
    Ok(())
}
