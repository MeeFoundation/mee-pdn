//! The stand's scenarios across containers, reached only over the
//! published HTTP port: no step reaches into a runtime, no store ticket
//! appears, and waiting for convergence is repeating the read. A ceremony
//! payload moves between nodes through the test, as a code moves between
//! two screens through a person. Ignored by default: `just test-docker`
//! builds the image and runs the suite.

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::RecordKind;
use pdn_node_http::shapes::{HeldPods, HostedIdentities};

mod common;
use common::{record_reads, record_route, Image, Stand};

/// A device joins an identity: the linking payload minted on the first
/// device is consumed on a second, which then hosts the identity and reads
/// a record its identity placed before it joined. Denied: the same payload
/// presented again is refused (its secret is burnt) and the presenter hosts
/// nothing; a node that never linked is refused as unknown — a node of its
/// own, since the bystander's refused attempt could leave a residue.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_device_joins_across_containers() -> Result<()> {
    let stand = Stand::new();
    let first = stand.spawn("first").await?;
    let second = stand.spawn("second").await?;
    let bystander = stand.spawn("bystander").await?;
    let stranger = stand.spawn("stranger").await?;

    let alice = first.create_identity().await?;
    let snoop = stranger.create_identity().await?;
    let own = first.create_pod(alice).await?;
    let before = first
        .place_record(alice, own, RecordKind::Claim, b"written before the link")
        .await?;

    let payload = first
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    // The budget of the whole act, named explicitly.
    second
        .post("/debug/link?timeout_secs=60", payload.clone())
        .await?
        .ok()?;

    // The second node hosts the identity now.
    let hosted: HostedIdentities = second.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&alice),
        "the linked node must report Alice: {hosted:?}"
    );

    // And reads what the first device placed before it joined, once the
    // armer has opened the pod.
    record_reads(&second, alice, own, before, b"written before the link")
        .await
        .context("the linked device did not catch up on the record placed before the link")?;

    // Denied (a replayed payload): a refusal, distinguishable from a node
    // that never reached the inviter.
    let refused = bystander.post("/debug/link", payload).await?;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a replayed linking payload must be refused, got {}: {}",
        refused.status,
        refused.text()
    );
    let nothing: HostedIdentities = bystander.get("/debug/identities").await?.json()?;
    assert!(
        nothing.identities.is_empty(),
        "a refused link must leave nothing behind: {nothing:?}"
    );

    // Denied (a node that never linked).
    let outsider = stranger.get(&record_route(snoop, own, before)).await?;
    assert_eq!(
        outsider.status,
        StatusCode::CONFLICT,
        "a node that never linked must be refused as unknown, got {}: {}",
        outsider.status,
        outsider.text()
    );
    Ok(())
}

/// A member keeps converging after one device of another member is
/// stopped: a sibling device of that member serves the pod, which is the
/// only place a member's whole device set is proven reachable across
/// processes. A failure to converge is not answered by a longer budget: the
/// reads before the stop show the sibling holding the pod, and which peers
/// Bob dialled comes out of his streamed log (`grep -o "peer=[0-9a-f]*" <log>
/// | sort | uniq -c`). The denial beside it: the failover must not widen
/// access.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_stopped_device_does_not_stop_its_pod() -> Result<()> {
    let stand = Stand::new();
    let first = stand.spawn("alice-first").await?;
    let sibling = stand.spawn("alice-second").await?;
    let bob_node = stand.spawn("bob").await?;
    let outsider = stand.spawn("outsider").await?;

    // The second device joins by linking.
    let alice = first.create_identity().await?;
    let payload = first
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    sibling
        .post("/debug/link?timeout_secs=60", payload)
        .await?
        .ok()?;

    // The pod, created and shared from the first device.
    let bob = bob_node.create_identity().await?;
    let snoop = outsider.create_identity().await?;
    let family = first.create_pod(alice).await?;
    let invite = first.invite_to_pod(alice, family).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;

    // Read by Bob and by the sibling, so the failover starts from a pod
    // that demonstrably reaches both.
    let placed = first
        .place_record(
            alice,
            family,
            RecordKind::Claim,
            b"placed from the first device",
        )
        .await?;
    record_reads(
        &bob_node,
        bob,
        family,
        placed,
        b"placed from the first device",
    )
    .await
    .context("Bob never read the record before the stop")?;
    record_reads(
        &sibling,
        alice,
        family,
        placed,
        b"placed from the first device",
    )
    .await
    .context("the sibling never held the pod before the stop")?;

    // Asked of the daemon, not the network: the released port can be
    // answered by a live node given the same port afterwards.
    first.stop().await?;
    assert!(
        !first.is_running().await?,
        "the device this scenario stops is still running"
    );

    // Bob converges on the device whose address the join handed him beside
    // the stopped one's.
    let served = sibling
        .place_record(alice, family, RecordKind::Claim, b"placed by the sibling")
        .await?;
    if let Err(err) = record_reads(&bob_node, bob, family, served, b"placed by the sibling").await {
        let logs = format!(
            "{}\n{}",
            bob_node.diagnostics().await,
            sibling.diagnostics().await
        );
        return Err(err.context(logs));
    }

    // Denied: the failover widened nothing.
    let refused = outsider.get(&record_route(snoop, family, served)).await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an outsider must stay refused after the failover, got {}: {}",
        refused.status,
        refused.text()
    );
    Ok(())
}

/// Two identities of one person on one node, each a member of a pod of its
/// own with a peer on another node: sharing a process is not sharing a pod.
/// The shared node lists each identity's own pod and not the other's, and
/// each peer reads the claim of the identity it shares its pod with.
/// Denied: the identity co-located with a pod's member on Alice's node, the
/// node that holds both pods, reads the other's pod; so, besides, do
/// neither peer nor a node that is no member of either — ordered after both
/// reads that prove the pods serve.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // two identities and two pods, kept in one place
async fn two_identities_on_one_node_keep_separate_pods() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let carol_node = stand.spawn("carol").await?;
    let outsider_node = stand.spawn("outsider").await?;

    // One node, two identities.
    let at_work = alice_node.create_identity().await?;
    let at_leisure = alice_node.create_identity().await?;
    assert_ne!(at_work, at_leisure);
    let hosted: HostedIdentities = alice_node.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&at_work) && hosted.identities.contains(&at_leisure),
        "the node must report both identities: {hosted:?}"
    );

    let bob = bob_node.create_identity().await?;
    let carol = carol_node.create_identity().await?;
    let outsider = outsider_node.create_identity().await?;

    // Each identity shares a pod of its own with its own peer, and places a
    // claim in it: different values, so a read that reached the wrong pod
    // answers the wrong bytes rather than nothing.
    let mut placed = Vec::new();
    for (identity, peer_node, peer, value) in [
        (at_work, &bob_node, bob, &b"alice@acme.example"[..]),
        (
            at_leisure,
            &carol_node,
            carol,
            &b"alice@bridgeclub.example"[..],
        ),
    ] {
        let pod = alice_node.create_pod(identity).await?;
        let invite = alice_node.invite_to_pod(identity, pod).await?;
        peer_node.join_pod(peer, invite).await?.ok()?;
        let claim = alice_node
            .place_record(identity, pod, RecordKind::Claim, value)
            .await?;
        placed.push((pod, claim));
    }
    let [(work_pod, work_claim), (club_pod, club_claim)] = placed[..] else {
        anyhow::bail!("two pods were placed");
    };

    // Allowed, both ways: each peer reads its own identity's claim.
    record_reads(&bob_node, bob, work_pod, work_claim, b"alice@acme.example")
        .await
        .context("Bob did not read the work identity's claim")?;
    record_reads(
        &carol_node,
        carol,
        club_pod,
        club_claim,
        b"alice@bridgeclub.example",
    )
    .await
    .context("Carol did not read the leisure identity's claim")?;

    // Read on Alice's node, where the route names which identity is asked.
    for (identity, own, other) in [
        (at_work, work_pod, club_pod),
        (at_leisure, club_pod, work_pod),
    ] {
        let held: HeldPods = alice_node
            .get(&format!("/debug/identities/{identity}/pods"))
            .await?
            .json()?;
        assert!(
            held.pods.contains(&own) && !held.pods.contains(&other),
            "{identity} must hold its own pod and not the other's: {held:?}"
        );
    }

    // Denied, both ways and to a third party: refused as unknown rather
    // than answered as absent.
    for (peer_node, acting, pod, claim, who) in [
        (
            &alice_node,
            at_work,
            club_pod,
            club_claim,
            "the work identity",
        ),
        (
            &alice_node,
            at_leisure,
            work_pod,
            work_claim,
            "the leisure identity",
        ),
        (&bob_node, bob, club_pod, club_claim, "Bob"),
        (&carol_node, carol, work_pod, work_claim, "Carol"),
        (
            &outsider_node,
            outsider,
            work_pod,
            work_claim,
            "the outsider",
        ),
        (
            &outsider_node,
            outsider,
            club_pod,
            club_claim,
            "the outsider",
        ),
    ] {
        let refused = peer_node.get(&record_route(acting, pod, claim)).await?;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{who} must be refused that pod, got {}: {}",
            refused.status,
            refused.text()
        );
    }
    Ok(())
}

/// A node started with `PDN_ANY_ORIGIN` answers a debug request with the
/// header a page of another origin needs to read it. Denied: a node started
/// without it sends none, so a page of another origin reads nothing of a
/// node nobody opened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn the_debug_surface_opens_to_any_origin_only_when_asked() -> Result<()> {
    const ORIGIN: &str = "access-control-allow-origin";
    let stand = Stand::new();
    let opened = stand
        .spawn_from("opened", Image::Stand, true, &[("PDN_ANY_ORIGIN", "1")])
        .await?;
    let closed = stand.spawn("closed").await?;

    let answer = opened.get("/debug/status").await?;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
    assert_eq!(
        answer.headers.get(ORIGIN).and_then(|v| v.to_str().ok()),
        Some("*"),
        "{:?}",
        answer.headers
    );
    // Denied (no flag).
    let answer = closed.get("/debug/status").await?;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
    assert_eq!(answer.headers.get(ORIGIN), None, "{:?}", answer.headers);
    Ok(())
}

/// A debug request naming another host in `Host` is refused, so a page made
/// same-origin by DNS rebinding is kept out. Paired: `localhost` is answered,
/// as the IP literal the suite's other requests name is.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn the_debug_surface_answers_an_ip_literal_or_localhost_alone() -> Result<()> {
    let stand = Stand::new();
    let node = stand.spawn("named").await?;
    let answer = node.get_as_host("/debug/status", "localhost").await?;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
    // Denied (a name).
    let answer = node
        .get_as_host("/debug/status", "evil.example:3011")
        .await?;
    assert_eq!(
        answer.status,
        StatusCode::MISDIRECTED_REQUEST,
        "{}",
        answer.text()
    );
    Ok(())
}
