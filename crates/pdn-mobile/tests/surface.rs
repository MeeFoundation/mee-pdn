//! The facade's surface, driven through exported calls alone.
//!
//! Every arrange and act step here is the one an application performs: a
//! membership comes from a minted pod invite consumed by the other handle,
//! and a wait is a repeated read. No test body holds a ticket — none
//! crosses this surface — and none reaches around the facade into the
//! runtime's own services.
//!
//! Two handles are 2 nodes in one process, which is weaker than the
//! container stand and stronger than nothing: the runtime's own suites
//! prove the inter-node path and the stand proves it across processes, so
//! what these add is the surface.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pdn_mobile::{PdnError, PdnNode, PodActRefusal, PodMembershipAct, PodRecordKind};
use pdn_node::PodInvite;
use tempfile::TempDir;
use test_utils::eventually;

/// A cadence a test can wait through.
const CADENCE_SECS: u64 = 1;

/// The bound a linking act gets when a test performs one.
const LINK_BUDGET_SECS: u64 = 30;

/// How long a test waits for a stale invite to reach its outcome — a dial
/// at an address whose node came back with no secret pending.
const STALE_INVITE_BOUND: Duration = Duration::from_secs(45);

/// One handle, its own node, its own directory.
struct Device {
    node: Arc<PdnNode>,
    /// Held so the directory outlives the node that writes in it.
    _dir: TempDir,
}

impl Device {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let node = PdnNode::new()?;
        node.bring_up(path_of(&dir), CADENCE_SECS).await?;
        Ok(Self { node, _dir: dir })
    }
}

fn path_of(dir: &TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

/// A payload minted here carries the runtime's own encoding, so a host of
/// another shape over the same runtime consumes what this one produced.
///
/// The bytes inside the code are what `pdn-node-http` takes as a request
/// body — the same serde encoding of the same runtime type. The assertion
/// goes through that encoding and back, and the payload it rebuilds is then
/// consumed for real: a divergence would otherwise be discovered by a
/// person in a room.
#[tokio::test]
async fn a_payload_crosses_in_the_runtimes_own_encoding() -> Result<()> {
    let inviter = Device::boot().await?;
    let scanner = Device::boot().await?;
    let alice = inviter.node.create_identity().await?;
    let bob = scanner.node.create_identity().await?;
    let pod = inviter.node.create_pod(alice.clone()).await?;

    let code = inviter
        .node
        .mint_pod_invite(alice, pod.clone(), Some(120))
        .await?;
    let bytes = URL_SAFE_NO_PAD.decode(&code)?;
    let as_another_host_reads_it: PodInvite = serde_json::from_slice(&bytes)?;
    assert_eq!(
        as_another_host_reads_it.pod.to_string(),
        pod,
        "the payload names the pod"
    );

    // Re-encoded by that other host's rules and consumed here: the round
    // trip is the interoperability, not the parse alone.
    let round_tripped = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&as_another_host_reads_it)?);
    assert_eq!(
        scanner.node.join_pod(bob.clone(), round_tripped).await?,
        pod
    );
    assert_eq!(scanner.node.pods(bob).await?, vec![pod]);

    for device in [inviter, scanner] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A pod member reads every record placed in the pod, and an identity that
/// never joined reads nothing of it.
///
/// Paired denial: the outsider, holding no invite, is refused the pod's
/// listing and its record. Beside it, the refusals a screen renders: a
/// claim takes no operation, a plain member removes nobody.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario, its denials in the same place
#[allow(clippy::similar_names)] // the roles of the scenario, named for what they are
async fn a_pod_member_reads_its_records_and_an_outsider_reads_nothing() -> Result<()> {
    let first_device = Device::boot().await?;
    let joiner = Device::boot().await?;
    let outsider = Device::boot().await?;
    let alice = first_device.node.create_identity().await?;
    let bob = joiner.node.create_identity().await?;
    let carol = outsider.node.create_identity().await?;

    let pod = first_device.node.create_pod(alice.clone()).await?;
    let claim = first_device
        .node
        .put_record(
            alice.clone(),
            pod.clone(),
            PodRecordKind::Claim,
            b"blood type 0+".to_vec(),
        )
        .await?;
    let code = first_device
        .node
        .mint_pod_invite(alice.clone(), pod.clone(), Some(120))
        .await?;
    assert_eq!(joiner.node.join_pod(bob.clone(), code).await?, pod);

    let read = eventually(|| async {
        let value = joiner
            .node
            .read_record(bob.clone(), pod.clone(), claim.clone())
            .await
            .unwrap_or(None);
        Ok(value.as_deref() == Some(b"blood type 0+".as_slice()))
    })
    .await;
    assert!(read.is_ok(), "the joiner reads the creator's claim");
    let mut members = joiner.node.pod_members(bob.clone(), pod.clone()).await?;
    members.sort_by(|a, b| a.identity.cmp(&b.identity));
    let mut expected = vec![(alice.clone(), true), (bob.clone(), false)];
    expected.sort();
    assert_eq!(
        members
            .iter()
            .map(|member| (member.identity.clone(), member.owner))
            .collect::<Vec<_>>(),
        expected,
        "the creator owns, the joiner is a plain member"
    );

    // Denied (outsider).
    let refused = outsider
        .node
        .list_records(carol.clone(), pod.clone())
        .await
        .expect_err("an outsider must not list the pod");
    assert!(
        matches!(refused, PdnError::UnknownPod { .. }),
        "{refused:?}"
    );
    let refused = outsider
        .node
        .read_record(carol, pod.clone(), claim.clone())
        .await
        .expect_err("an outsider must not read the claim");
    assert!(
        matches!(refused, PdnError::UnknownPod { .. }),
        "{refused:?}"
    );

    let refused = joiner
        .node
        .append_op(bob.clone(), pod.clone(), claim, b"edit".to_vec())
        .await
        .expect_err("a claim is placed once");
    assert!(
        matches!(refused, PdnError::RecordPlacedOnce { .. }),
        "{refused:?}"
    );
    let refused = joiner
        .node
        .pod_act(bob, pod, PodMembershipAct::Remove { member: alice })
        .await
        .expect_err("a plain member removes nobody");
    assert!(
        matches!(
            refused,
            PdnError::ActRefused {
                reason: PodActRefusal::NotAnOwner
            }
        ),
        "{refused:?}"
    );

    for device in [first_device, joiner, outsider] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A code read for the wrong act reaches the runtime and comes back as the
/// runtime's refusal, because nothing above the facade parses a payload to
/// tell the two acts apart.
#[tokio::test]
async fn a_code_read_for_the_wrong_act_is_refused_by_the_runtime() -> Result<()> {
    let inviter = Device::boot().await?;
    let scanner = Device::boot().await?;
    let alice = inviter.node.create_identity().await?;
    let bob = scanner.node.create_identity().await?;

    // A device-linking code, read on the screen that joins a pod.
    let linking = inviter
        .node
        .mint_linking_payload(alice.clone(), Some(120))
        .await?;
    let wrong_act = scanner
        .node
        .join_pod(bob.clone(), linking)
        .await
        .expect_err("a join must not consume a linking payload");
    assert!(
        matches!(wrong_act, PdnError::MalformedInput { .. }),
        "the two payload kinds do not decode as one another: {wrong_act:?}"
    );

    // And the other way round, with the budget the linking act takes.
    let pod = inviter.node.create_pod(alice.clone()).await?;
    let invite = inviter.node.mint_pod_invite(alice, pod, Some(120)).await?;
    let wrong_act = scanner
        .node
        .consume_linking_payload(invite, LINK_BUDGET_SECS)
        .await
        .expect_err("a linking call must not consume a pod invite");
    assert!(
        matches!(wrong_act, PdnError::MalformedInput { .. }),
        "{wrong_act:?}"
    );
    assert_eq!(
        scanner.node.pods(bob).await?,
        Vec::<String>::new(),
        "neither refusal left a pod behind"
    );

    for device in [inviter, scanner] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A bring-up nobody is waiting for any more leaves the handle usable, and
/// a stop that overtakes one leaves no node running.
///
/// Both are the same hazard from 2 sides: a shell that leaves the
/// foreground mid-bring-up, and a coroutine cancelled under it. Whichever
/// way the race falls, what must not happen is a handle stuck between its
/// states, or a node nobody holds keeping the directory — the next
/// bring-up would then be refused by a node its own caller had stopped.
#[tokio::test]
async fn a_bring_up_nobody_awaits_leaves_the_handle_usable() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // A stop issued while a bring-up is in flight.
    let node = PdnNode::new()?;
    let bringing = {
        let node = Arc::clone(&node);
        let path = path_of(&dir);
        tokio::spawn(async move { node.bring_up(path, CADENCE_SECS).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    node.stop().await?;
    let brought = bringing.await?;
    assert!(
        matches!(brought, Ok(()) | Err(PdnError::NodeNotUp)),
        "a bring-up a stop overtook either finished before it or reports the node not up: {brought:?}"
    );
    let after = node
        .create_identity()
        .await
        .expect_err("the stop must leave no node behind, whichever way the race fell");
    assert!(matches!(after, PdnError::NodeNotUp), "{after:?}");

    // And the directory is free, which is what says the overtaken node was
    // shut down rather than left running with nobody holding it.
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;
    node.stop().await?;

    // A bring-up whose caller goes away.
    let abandoned = PdnNode::new()?;
    let cancelled = {
        let node = Arc::clone(&abandoned);
        let path = path_of(&dir);
        tokio::spawn(async move { node.bring_up(path, CADENCE_SECS).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancelled.abort();
    // The handle is not stranded mid-transition: a stop and a bring-up
    // reach a running node again.
    abandoned.stop().await?;
    abandoned.bring_up(path_of(&dir), CADENCE_SECS).await?;
    let identity = abandoned.create_identity().await?;
    assert!(
        !identity.is_empty(),
        "the handle came back to a running node"
    );
    abandoned.stop().await?;
    Ok(())
}

/// One handle owns one node: bring-up and stop are explicit, a stop is safe
/// to repeat, a second bring-up is refused rather than replacing the node,
/// a call with nothing up is refused as that, and a directory a running
/// node holds is refused as held.
#[tokio::test]
async fn one_handle_owns_one_node() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = PdnNode::new()?;

    let before = node
        .create_identity()
        .await
        .expect_err("a call before a bring-up must be refused");
    assert!(matches!(before, PdnError::NodeNotUp), "{before:?}");

    node.bring_up(path_of(&dir), CADENCE_SECS).await?;
    let again = node
        .bring_up(path_of(&dir), CADENCE_SECS)
        .await
        .expect_err("a second bring-up must be refused");
    assert!(matches!(again, PdnError::NodeAlreadyUp), "{again:?}");

    // A second handle on the same directory: one directory belongs to one
    // running node, and this is reported as that rather than as a corrupt
    // store.
    let sibling = PdnNode::new()?;
    let held = sibling
        .bring_up(path_of(&dir), CADENCE_SECS)
        .await
        .expect_err("a directory a running node holds must be refused");
    assert!(matches!(held, PdnError::StorageHeld), "{held:?}");

    node.stop().await?;
    node.stop().await?;
    let after = node
        .create_identity()
        .await
        .expect_err("a call after a stop must be refused");
    assert!(matches!(after, PdnError::NodeNotUp), "{after:?}");

    // With the first node down, the directory is free for the next one.
    sibling.bring_up(path_of(&dir), CADENCE_SECS).await?;
    sibling.stop().await?;
    Ok(())
}

/// A node comes back on its directory as itself: the same node id, the same
/// hosted identity, and its pod and records readable at once with no
/// ceremony repeated. What does not come back is work in flight — a pod
/// invite minted before the stop is refused after it.
#[tokio::test]
async fn a_node_comes_back_on_its_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = PdnNode::new()?;
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;

    let alice = node.create_identity().await?;
    let node_id = node.node_id().await?;
    let pod = node.create_pod(alice.clone()).await?;
    let claim = node
        .put_record(
            alice.clone(),
            pod.clone(),
            PodRecordKind::Claim,
            b"a@example".to_vec(),
        )
        .await?;
    let minted = node
        .mint_pod_invite(alice.clone(), pod.clone(), Some(600))
        .await?;

    node.stop().await?;
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;

    assert_eq!(node.node_id().await?, node_id, "the same node answers");
    assert_eq!(
        node.hosted_identities().await?,
        vec![alice.clone()],
        "the identity it hosted is hosted again, with no ceremony repeated"
    );
    assert_eq!(
        node.pods(alice.clone()).await?,
        vec![pod.clone()],
        "its pod is held again"
    );
    assert_eq!(
        node.read_record(alice.clone(), pod, claim)
            .await?
            .as_deref(),
        Some(b"a@example".as_slice()),
        "its own records read at once"
    );

    // The pod invite was work in flight, and work in flight does not come back.
    // Which of the 3 ceremony outcomes it is depends on what the restart did
    // to the address the payload carries — the node id survives the
    // directory, the port the endpoint binds need not — so the assertion
    // names the set and excludes the 2 answers that would be wrong however
    // that went: a success, and an unrecognized failure.
    let scanner = Device::boot().await?;
    let bob = scanner.node.create_identity().await?;
    let stale = tokio::time::timeout(STALE_INVITE_BOUND, scanner.node.join_pod(bob, minted))
        .await?
        .expect_err("a pod invite minted before the stop must not work after it");
    assert!(
        matches!(
            stale,
            PdnError::CeremonyRefused
                | PdnError::DialogueTimedOut
                | PdnError::CounterpartyUnreachable
        ),
        "the refusal is one the table names rather than an unrecognized failure: {stale:?}"
    );

    node.stop().await?;
    scanner.node.stop().await?;
    Ok(())
}
