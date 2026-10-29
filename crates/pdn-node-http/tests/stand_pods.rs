//! The pod scenario across containers, reached only over the published
//! HTTP port: an invite moves between nodes through the test, and waiting
//! for convergence is repeating the read. Its restart arms are in
//! `stand_restart.rs`. Ignored by default: `just test-docker` builds the
//! image and runs the suite.

use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::{PdnId, RecordKind};
use pdn_node_http::shapes::{Act, HeldPod, HeldPods, InviteState, Member};

mod common;
use common::{
    eventually, members_read, ops_read, pod_listed, pod_route, record_reads, record_route, Host,
    Stand,
};

/// What the serving half's mark takes after its last write, many times over.
const USED_WATCH: Duration = Duration::from_secs(30);

/// Several times what an announced write takes to reach a member device,
/// which the remaining member's read of the later record has just shown.
const REMOVED_WATCH: Duration = Duration::from_secs(5);

fn member(id: PdnId, owner: bool) -> Member {
    Member { id, owner }
}

/// A pod runs across three containers over HTTP alone: any member invites,
/// a record and an edit reach every member, an owner's promotion and removal
/// take effect, and the removed member stops receiving. Paired denials: a
/// consumed invite, a plain member's owner-only acts beside the owner's
/// promotion, and the removed member's reads beside the remaining member's.
/// The consumed invite is presented by an identity of Carol's node that is
/// no member — the one a live secret would admit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one scenario, with its denials in the same place
async fn a_pod_runs_across_three_containers() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let carol_node = stand.spawn("carol").await?;
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let carol = carol_node.create_identity().await?;
    let carol_work = carol_node.create_identity().await?;

    // The creator invites Bob, and Bob invites Carol.
    let family = alice_node.create_pod(alice).await?;
    let invite = alice_node.invite_to_pod(alice, family).await?;
    let joined: HeldPod = bob_node.join_pod(bob, invite).await?.json()?;
    assert_eq!(joined.pod, family);
    let invite = bob_node.invite_to_pod(bob, family).await?;
    let joined: HeldPod = carol_node.join_pod(carol, invite.clone()).await?.json()?;
    assert_eq!(joined.pod, family);
    let nodes = [(&alice_node, alice), (&bob_node, bob), (&carol_node, carol)];
    let three = [
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    for (node, identity) in nodes {
        members_read(node, identity, family, &three).await?;
    }

    // Denied (a consumed invite).
    let replayed = carol_node.join_pod(carol_work, invite).await?;
    assert_eq!(
        replayed.status,
        StatusCode::FORBIDDEN,
        "a consumed invite must be refused, got {}: {}",
        replayed.status,
        replayed.text()
    );
    let held: HeldPods = carol_node
        .get(&format!("/debug/identities/{carol_work}/pods"))
        .await?
        .json()?;
    assert!(
        held.pods.is_empty(),
        "a refused join must leave no pod behind: {held:?}"
    );
    members_read(&bob_node, bob, family, &three).await?;

    let claim = alice_node
        .place_record(alice, family, RecordKind::Claim, b"birthday 12 May")
        .await?;
    let note = alice_node
        .place_record(alice, family, RecordKind::MergeableDocument, b"milk")
        .await?;
    // Carol's edit waits for the note to read on her replica: an operation
    // on a record that reads on nothing is refused.
    ops_read(&carol_node, carol, family, note, &[(alice, b"milk")]).await?;
    carol_node
        .append_op(carol, family, note, b"eggs")
        .await?
        .ok()?;
    let edited: [(PdnId, &[u8]); 2] = [(alice, b"milk"), (carol, b"eggs")];
    for (node, identity) in nodes {
        record_reads(node, identity, family, claim, b"birthday 12 May").await?;
        ops_read(node, identity, family, note, &edited).await?;
    }

    // Denied (a plain member's owner-only acts). Beside them, the owner's
    // promotion of Bob takes effect on every node.
    for act in [Act::Promote(carol), Act::Remove(bob)] {
        let refused = carol_node.pod_act(carol, family, act).await?;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "a plain member's {act:?} must be refused, got {}: {}",
            refused.status,
            refused.text()
        );
    }
    alice_node
        .pod_act(alice, family, Act::Promote(bob))
        .await?
        .ok()?;
    let promoted = [member(alice, true), member(bob, true), member(carol, false)];
    for (node, identity) in nodes {
        members_read(node, identity, family, &promoted).await?;
    }

    bob_node
        .pod_act(bob, family, Act::Remove(carol))
        .await?
        .ok()?;
    // Placed before Alice's replica holds the removal, a record would rightly
    // still be served to Carol.
    members_read(
        &alice_node,
        alice,
        family,
        &[member(alice, true), member(bob, true)],
    )
    .await?;
    let first = alice_node
        .place_record(alice, family, RecordKind::Claim, b"after the removal")
        .await?;
    let second = alice_node
        .place_record(
            alice,
            family,
            RecordKind::Claim,
            b"after the removal, again",
        )
        .await?;
    record_reads(&bob_node, bob, family, first, b"after the removal").await?;
    record_reads(&bob_node, bob, family, second, b"after the removal, again").await?;

    // Denied (the removed member): absent or refused, never read.
    let watched_until = tokio::time::Instant::now() + REMOVED_WATCH;
    while tokio::time::Instant::now() < watched_until {
        for record in [first, second] {
            let answer = carol_node.get(&record_route(carol, family, record)).await?;
            assert!(
                matches!(answer.status, StatusCode::NOT_FOUND | StatusCode::CONFLICT),
                "the removed member answered {} for {record:?}: {}",
                answer.status,
                answer.text()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Carol's device learns of its removal.
    pod_listed(&carol_node, carol, family, false).await?;
    let refused = carol_node
        .get(&format!("{}/members", pod_route(carol, family)))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "the removed member's requests on the pod must be refused, got {}: {}",
        refused.status,
        refused.text()
    );

    Ok(())
}

/// The state of `code` as `host` answers it at `route`.
async fn state_of(host: &Host, route: &str, code: &Bytes) -> Result<InviteState> {
    host.post(route, code.clone()).await?.json()
}

/// Whether `host` reads `code` used by `by` within the watch.
async fn reads_used(host: &Host, route: &str, code: &Bytes, by: String) -> Result<bool> {
    let used = InviteState::Used { by };
    let seen = eventually(USED_WATCH, || async {
        Ok((state_of(host, route, code).await? == used).then_some(()))
    })
    .await?;
    Ok(seen.is_some())
}

/// An invite's state is read over HTTP alone: a pod invite and a linking
/// invite minted on one host read pending before a second host consumes
/// each, and used afterwards, naming the joiner and the newcomer's node id.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn an_invites_state_is_read_over_http() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let family = alice_node.create_pod(alice).await?;

    let invite = alice_node.invite_to_pod(alice, family).await?;
    let pod_state = format!("/debug/identities/{alice}/pods/invite-state");
    assert_eq!(
        state_of(&alice_node, &pod_state, &invite).await?,
        InviteState::Pending
    );
    let joined: HeldPod = bob_node.join_pod(bob, invite.clone()).await?.json()?;
    assert_eq!(joined.pod, family);
    assert!(
        reads_used(&alice_node, &pod_state, &invite, bob.to_string()).await?,
        "the pod invite never read used by the joiner: {:?}",
        state_of(&alice_node, &pod_state, &invite).await
    );

    let payload = alice_node
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    let linking_state = "/debug/linking-invite-state";
    assert_eq!(
        state_of(&alice_node, linking_state, &payload).await?,
        InviteState::Pending
    );
    bob_node
        .post("/debug/link?timeout_secs=60", payload.clone())
        .await?
        .ok()?;
    let newcomer = bob_node.node_id().await.context("the newcomer's node id")?;
    assert!(
        reads_used(&alice_node, linking_state, &payload, newcomer).await?,
        "the linking invite never read used by the newcomer: {:?}",
        state_of(&alice_node, linking_state, &payload).await
    );
    Ok(())
}
