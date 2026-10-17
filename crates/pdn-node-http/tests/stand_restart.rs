//! The stand's restart scenarios: a node's container stopped — cleanly and
//! by a kill with no grace — and started again on its own state directory,
//! asserted to come back as the same node; a kill landing in the middle of
//! a stream of acknowledged placements; a state directory that fills; and
//! a member's node coming back with its pod, a pod it left staying left.
//!
//! A restart is asserted here and nowhere else: an in-process respawn
//! proves nothing about a process that exits, and only a container carries
//! its filesystem across one.
//!
//! Ignored by default: the suite needs a container daemon and a built image,
//! and `just test-docker` builds the image and runs it.

use std::sync::Arc;

use anyhow::{ensure, Context as _, Result};
use axum::http::StatusCode;
use pdn_node::{PdnId, PodId, RecordKind, RecordRef};
use pdn_node_http::shapes::{Act, HeldPods, HostedIdentities, Member, Records};

mod common;
use common::{
    body, eventually, members_read, pod_listed, pod_route, record_reads, record_reads_within,
    record_route, Host, Stand, CONVERGENCE_BUDGET,
};

/// This node's wire id, read from the debug status page — the first line
/// is `node <id>`.
async fn node_id(host: &Host) -> Result<String> {
    let status = host.get("/debug/status").await?.ok()?;
    let text = String::from_utf8_lossy(&status);
    text.lines()
        .find_map(|line| line.strip_prefix("node "))
        .map(str::to_owned)
        .context("the status page carries no node line")
}

/// The recovery assertion set, identical for the clean-stop arm and the
/// kill arm on purpose: recovery that differed by the manner of stopping
/// would depend on the previous process having said goodbye, which a kill
/// does not provide. Same node id, the identity hosted, the pod listed,
/// `previous` — placed and acknowledged before the stop — still readable,
/// the other member still listing the identity, and that member converging
/// on `sentinel`, placed after the restart with no invite minted and no
/// linking performed. Returns the sentinel's record.
#[allow(clippy::too_many_arguments)] // the two nodes, their two identities, the pod and both records
async fn assert_recovered(
    member: &Host,
    id_before: &str,
    alice: PdnId,
    bob: PdnId,
    other: &Host,
    family: PodId,
    previous: (RecordRef, &'static [u8]),
    sentinel: &'static [u8],
) -> Result<RecordRef> {
    ensure!(
        node_id(member).await? == id_before,
        "the node id must survive the restart"
    );
    let hosted: HostedIdentities = member.get("/debug/identities").await?.json()?;
    ensure!(
        hosted.identities.contains(&alice),
        "the identity must be hosted again: {hosted:?}"
    );
    pod_listed(member, alice, family, true)
        .await
        .context("the pod must be listed again")?;
    // What was acknowledged before the stop reads back — polled, because
    // the pod reopens on the armer's first sweep.
    let (record, payload) = previous;
    record_reads(member, alice, family, record, payload)
        .await
        .context("the record acknowledged before the stop did not read back")?;
    members_read(
        other,
        bob,
        family,
        &[
            Member {
                id: alice,
                owner: true,
            },
            Member {
                id: bob,
                owner: false,
            },
        ],
    )
    .await
    .context("the other member must still list the restarted node's identity")?;
    // The recovered pod carries a fresh record to the other member — no
    // ceremony was repeated: nothing since the restart minted an invite or
    // linked a device. Placed this soon after the start, it can be announced
    // before the restarted node's swarm forms; the other member's dial on its
    // new neighbor brings it then, killed or stopped, long before a pod pass.
    let placed = member
        .place_record(alice, family, RecordKind::Claim, sentinel)
        .await?;
    record_reads_within(other, bob, family, placed, sentinel, CONVERGENCE_BUDGET)
        .await
        .context("the other member did not converge with the restarted node")?;
    Ok(placed)
}

/// How a restart scenario stops its node.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// The daemon's stop: the process leaves by its own shutdown path.
    Clean,
    /// `SIGKILL`, no grace: a process that ends without warning.
    Kill,
}

/// The restart scenario of either arm, with its denial: a node holding an
/// identity that is a member of a pod is stopped and started again, the
/// recovery asserted the same way for both, and a node started from the
/// same image on an empty state directory holds none of it. Without that
/// last arm the scenario would pass just as well against a node that
/// quietly re-created everything.
async fn restart_scenario(stop: Stop) -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;

    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let family = alice_node.create_pod(alice).await?;
    let invite = alice_node.invite_to_pod(alice, family).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;
    let before = alice_node
        .place_record(alice, family, RecordKind::Claim, b"before the stop")
        .await?;
    record_reads(&bob_node, bob, family, before, b"before the stop")
        .await
        .context("the other member never read the record before the stop")?;
    let id_before = node_id(&alice_node).await?;

    match stop {
        Stop::Clean => {
            // Asserted as clean: only the exit code and the time it took tell
            // the graceful path from one the daemon cut short.
            let stopping = std::time::Instant::now();
            alice_node.stop().await?;
            let stop_took = stopping.elapsed();
            ensure!(
                !alice_node.is_running().await?,
                "the stopped container must be down before the start"
            );
            let exit_code = alice_node.exit_code()?;
            ensure!(
                exit_code == 0,
                "the clean stop must end the process by its own shutdown path; \
                 it exited {exit_code} (137 is the daemon's kill after the grace)"
            );
            ensure!(
                stop_took < CLEAN_STOP_BUDGET,
                "the clean stop took {stop_took:?}, which leaves no room inside the \
                 daemon's ten-second grace: the drain and the runtime's own cleanup \
                 together have to finish well inside it"
            );
        }
        Stop::Kill => {
            // The settle window and the address book's first write first; the
            // mid-stream scenario below is where the window itself is the
            // subject.
            tokio::time::sleep(SETTLE_WINDOW.max(ADDRESS_BOOK_WRITTEN)).await;
            alice_node.kill()?;
            ensure!(
                !alice_node.is_running().await?,
                "the killed container must be down before the start"
            );
        }
    }
    alice_node.start().await?;
    assert_recovered(
        &alice_node,
        &id_before,
        alice,
        bob,
        &bob_node,
        family,
        (before, b"before the stop"),
        b"after the restart",
    )
    .await
    .with_context(|| format!("recovery after the {stop:?} stop"))?;

    // Denial: the same image on an empty state directory has a node id of
    // its own, hosts nothing, and refuses reads addressed to the restarted
    // node's identity.
    let fresh = stand.spawn("fresh").await?;
    ensure!(
        node_id(&fresh).await? != id_before,
        "a fresh directory must be a different node"
    );
    let hosted: HostedIdentities = fresh.get("/debug/identities").await?.json()?;
    ensure!(
        hosted.identities.is_empty(),
        "a fresh node must host nothing: {hosted:?}"
    );
    let refused = fresh.get(&record_route(alice, family, before)).await?;
    ensure!(
        refused.status == StatusCode::CONFLICT,
        "a fresh node must refuse the identity as unknown, got {}: {}",
        refused.status,
        refused.text()
    );
    Ok(())
}

/// A node holding an identity that is a member of a pod, stopped cleanly
/// and started again, comes back as the same node; a fresh one holds
/// nothing of it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_stopped_node_is_the_same_node_and_a_fresh_one_holds_nothing() -> Result<()> {
    restart_scenario(Stop::Clean).await
}

/// The same node killed with no grace comes back the same way: recovery
/// that differed by the manner of stopping would depend on a goodbye a
/// kill does not provide.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_killed_node_is_the_same_node_and_a_fresh_one_holds_nothing() -> Result<()> {
    restart_scenario(Stop::Kill).await
}

/// How long after its acknowledgement a write is committed for certain:
/// the replica store flushes its write transaction every 500 milliseconds
/// and the blob store commits when its batch closes, independently — so a
/// kill inside the window can keep the entry record while losing its
/// payload, which reads as absent. Tightening it is how this scenario would
/// catch a store whose commit cadence regressed.
const SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// A node writes its address book every 10 s, and a shutdown writes it last:
/// killed before the first write, a node on direct connectivity comes back
/// knowing no peer's address, and its peers dial its old port.
const ADDRESS_BOOK_WRITTEN: std::time::Duration = std::time::Duration::from_secs(12);

/// Readiness meets the blob store the refused placement stopped at its next
/// poll; the margin covers a poll waiting out the host's runtime budget.
const FULL_STORE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// The daemon kills what is still running about ten seconds after its stop
/// signal; the drain budget and the runtime's cleanup budget both count.
const CLEAN_STOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Ten acknowledged writes take well under a second on a node that answers.
const STREAM_BUDGET: std::time::Duration = std::time::Duration::from_mins(1);

/// A kill mid-stream: every record whose placement was acknowledged before
/// the settle window reads back whole, and a record cut inside it is absent
/// or whole, never torn and never a read error — durability starts one
/// window after the acknowledgement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one kill, the settled and the cut placements in the same place
async fn a_kill_mid_stream_loses_nothing_settled() -> Result<()> {
    let stand = Stand::new();
    let node = Arc::new(stand.spawn("writer").await?);
    let alice = node.create_identity().await?;
    let stream = node.create_pod(alice).await?;

    // The writer places records and logs each acknowledgement's instant
    // until a placement fails under it — the kill below lands while it runs.
    // Values differ per record, so a torn value cannot borrow a neighbour's
    // bytes.
    let acked: Arc<std::sync::Mutex<Vec<(std::time::Instant, RecordRef)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = {
        let node = Arc::clone(&node);
        let acked = Arc::clone(&acked);
        tokio::spawn(async move {
            let mut index = 0usize;
            loop {
                let placed = node
                    .place_record(
                        alice,
                        stream,
                        RecordKind::Claim,
                        format!("value-{index:04}").as_bytes(),
                    )
                    .await;
                match placed {
                    Ok(record) => {
                        if let Ok(mut log) = acked.lock() {
                            log.push((std::time::Instant::now(), record));
                        }
                        index += 1;
                    }
                    // The kill cut this placement before its acknowledgement.
                    Err(_) => return index,
                }
            }
        })
    };

    // The kill lands once the stream has flowed for longer than the settle
    // window, so placements older than the window demonstrably exist. The
    // wait is bounded: the writer stops at its first failed placement, so a
    // stream that dies early stops the counter for good, and without a
    // deadline this loop would spin forever rather than report what it saw.
    let streaming_since = std::time::Instant::now();
    loop {
        let count = acked
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("ack log poisoned"))?
            .len();
        if count >= 10 && streaming_since.elapsed() > SETTLE_WINDOW * 2 {
            break;
        }
        ensure!(
            streaming_since.elapsed() < STREAM_BUDGET,
            "the stream never reached ten acknowledgements: {count} in {:?}",
            streaming_since.elapsed()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let killed_at = std::time::Instant::now();
    node.kill()?;
    // Bounded for the same reason the client is: a request cut by the kill
    // can be one the daemon leaves unanswered, and an unbounded join would
    // wedge the run instead of failing it.
    let cut_at = tokio::time::timeout(STREAM_BUDGET, writer)
        .await
        .context("the writer never returned after the kill")??;
    let log = acked
        .lock()
        .map_err(|_poisoned| anyhow::anyhow!("ack log poisoned"))?
        .clone();
    let settled = log
        .iter()
        .filter(|(instant, _record)| killed_at.duration_since(*instant) > SETTLE_WINDOW)
        .count();
    ensure!(
        settled >= 1,
        "at least one placement must have settled before the kill"
    );

    node.start().await?;
    // Every settled record reads back whole. The first read waits out the
    // pod reopening; the rest are local.
    for (index, (_instant, record)) in log.iter().enumerate().take(settled) {
        record_reads(
            &node,
            alice,
            stream,
            *record,
            format!("value-{index:04}").as_bytes(),
        )
        .await
        .with_context(|| format!("record {index} settled before the kill and must read"))?;
    }
    // Everything the kill cut inside the window — acknowledged records
    // younger than it, and the one in flight, which only a listing can name
    // — is absent or whole, never partial and never an error the surface
    // cannot name.
    let listed: Records = node
        .get(&format!("{}/records", pod_route(alice, stream)))
        .await?
        .json()?;
    let acknowledged: Vec<RecordRef> = log.iter().map(|(_instant, record)| *record).collect();
    let cut = log
        .iter()
        .enumerate()
        .skip(settled)
        .map(|(index, (_instant, record))| (*record, Some(index)))
        .chain(
            listed
                .records
                .iter()
                .filter(|record| !acknowledged.contains(record))
                .map(|record| (*record, None)),
        );
    for (record, index) in cut {
        let answer = node.get(&record_route(alice, stream, record)).await?;
        let whole = answer.status == StatusCode::OK
            && index.map_or(
                answer.body == format!("value-{cut_at:04}").as_bytes(),
                |index| answer.body == format!("value-{index:04}").as_bytes(),
            );
        let absent = answer.status == StatusCode::NOT_FOUND;
        ensure!(
            whole || absent,
            "a record cut inside the settle window must be absent or whole, \
             got {} for {record:?}: {}",
            answer.status,
            answer.text()
        );
    }
    Ok(())
}

/// A storage failure arrives as a failure: a node whose state directory is
/// a size-bounded filesystem refuses a placement once the store is full —
/// as a failed request, not a success — and no record the pod lists holds
/// the refused value. On an in-memory node a full disk is invisible; on a
/// device it is the ordinary way storage fails.
///
/// The pod is shared with a member on another node, as a pod is: the
/// placement fails at the blob store, which stops, while the replica store
/// keeps answering until a commit of its own meets the full disk —
/// readiness reads both.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one fill, the refusal, the store's failure and the probes
async fn a_full_state_directory_refuses_writes_loudly() -> Result<()> {
    let stand = Stand::new();
    // Room for the stores to provision and a handful of records, small
    // enough that the fill below overruns it several times over.
    let node = stand
        .spawn_with_bounded_state("bounded", 16 * 1024 * 1024)
        .await?;
    let bob_node = stand.spawn("bob").await?;
    let alice = node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let family = node.create_pod(alice).await?;
    let invite = node.invite_to_pod(alice, family).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;
    let records = format!("{}/records?kind=claim", pod_route(alice, family));

    // Each payload is distinct — the blob store is content-addressed, and
    // one payload placed 256 times is one blob, which no bound ever meets.
    let payload_for = |index: usize| {
        let mut payload = vec![0xa5u8; 256 * 1024];
        payload.splice(..8, index.to_be_bytes());
        payload
    };
    let mut refused = None;
    // 256 placements of 256 KiB is four times the mount: the loop must meet
    // the refusal long before it runs out.
    for index in 0..256usize {
        let answer = node.post(&records, payload_for(index)).await?;
        if !answer.status.is_success() {
            refused = Some((index, answer));
            break;
        }
    }
    let Some((refused_index, answer)) = refused else {
        anyhow::bail!("the store absorbed four times its filesystem's size without refusing");
    };
    ensure!(
        answer.status.is_client_error() || answer.status.is_server_error(),
        "the refusal must be an error status, got {}",
        answer.status
    );

    // The refused placement is not stored: no record the pod lists reads
    // its payload as if it were.
    let listed = node
        .get(&format!("{}/records", pod_route(alice, family)))
        .await?;
    if listed.status == StatusCode::OK {
        let listed: Records = listed.json()?;
        for record in listed.records {
            let read = node.get(&record_route(alice, family, record)).await?;
            ensure!(
                !(read.status == StatusCode::OK && read.body == payload_for(refused_index)),
                "a refused placement must not read back as stored"
            );
        }
    }

    // What the full filesystem does to the node as a whole shows on the next
    // operation the replica store has to commit; the store then refuses
    // everything until a restart reopens it.
    let created = node.post("/debug/identities", body(b"")).await?;
    ensure!(
        !created.status.is_success(),
        "provisioning an identity on a full state directory must fail, got {}",
        created.status
    );

    // Readiness reads both stores, turning a silent zombie into an outage a
    // platform can restart; the stopped blob store answers for the full disk
    // while the replica store may still read. Bob keeps placing records, so
    // the full node meets the pod's traffic while it is polled.
    let mut placed = 0u32;
    let unready = eventually(FULL_STORE_BUDGET, || {
        placed += 1;
        let claim = format!("placed by Bob, {placed}");
        let (bob_node, node) = (&bob_node, &node);
        async move {
            let _record = bob_node
                .place_record(bob, family, RecordKind::Claim, claim.as_bytes())
                .await?;
            let answer = node.get("/ready").await?;
            Ok((!answer.status.is_success()).then_some(answer.status))
        }
    })
    .await?;
    ensure!(
        unready.is_some(),
        "a node whose store stopped answering must stop reporting itself ready"
    );
    let live = node.get("/live").await?;
    ensure!(
        live.status.is_success(),
        "liveness must still answer: the process is up, whatever its store says"
    );
    Ok(())
}

/// A member's node stopped and started again on its state directory hosts
/// its pod again and reads a record placed while it was down, with no
/// invite minted after the restart; a second pod it left before another
/// restart stays left. Paired denial: a request addressing the left pod is
/// refused beside the same request on the kept pod answering.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_member_node_comes_back_with_its_pod_and_a_left_pod_stays_left() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let family = alice_node.create_pod(alice).await?;
    let invite = alice_node.invite_to_pod(alice, family).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;

    bob_node.stop().await?;
    let meanwhile = alice_node
        .place_record(alice, family, RecordKind::Claim, b"meanwhile")
        .await?;
    bob_node.start().await?;
    pod_listed(&bob_node, bob, family, true).await?;
    record_reads(&bob_node, bob, family, meanwhile, b"meanwhile")
        .await
        .context("the record placed while the member was down never reached it")?;

    let wedding = alice_node.create_pod(alice).await?;
    let invite = alice_node.invite_to_pod(alice, wedding).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;
    bob_node.pod_act(bob, wedding, Act::Leave).await?.ok()?;
    bob_node.stop().await?;
    bob_node.start().await?;
    // The kept pod served again first: the recovery that would bring the
    // left one back has run.
    let family_members = [
        Member {
            id: alice,
            owner: true,
        },
        Member {
            id: bob,
            owner: false,
        },
    ];
    members_read(&bob_node, bob, family, &family_members).await?;
    let held: HeldPods = bob_node
        .get(&format!("/debug/identities/{bob}/pods"))
        .await?
        .json()?;
    ensure!(
        !held.pods.contains(&wedding),
        "the left pod came back with the restart: {held:?}"
    );
    // Denied (the left pod).
    let left = bob_node
        .get(&format!("{}/members", pod_route(bob, wedding)))
        .await?;
    ensure!(
        left.status == StatusCode::CONFLICT,
        "a pod left before the restart must be refused, got {}: {}",
        left.status,
        left.text()
    );
    Ok(())
}
