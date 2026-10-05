//! The cell scenario across containers, reached only over the published
//! HTTP port: an invite moves between nodes through the test, and waiting
//! for convergence is repeating the read. Its restart arms are in
//! `stand_restart.rs`. Ignored by default: `just test-docker` builds the
//! image and runs the suite.

use std::time::Duration;

use anyhow::Result;
use axum::http::StatusCode;
use pdn_node::{PdnId, RecordKind};
use pdn_node_http::shapes::{Act, HeldCell, HeldCells, Member};

mod common;
use common::{cell_listed, cell_route, members_read, ops_read, record_reads, record_route, Stand};

/// Several times what an announced write takes to reach a member device,
/// which the remaining member's read of the later record has just shown.
const KICKED_WATCH: Duration = Duration::from_secs(5);

fn member(id: PdnId, owner: bool) -> Member {
    Member { id, owner }
}

/// A cell runs across three containers over HTTP alone: any member invites,
/// a record and an edit reach every member, an owner's promotion and kick
/// take effect, and the kicked member stops receiving. Paired denials: a
/// consumed invite, a plain member's owner-only acts beside the owner's
/// promotion, and the kicked member's reads beside the remaining member's.
/// The consumed invite is presented by an identity of Carol's node that is
/// no member — the one a live secret would admit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one scenario, with its denials in the same place
async fn a_cell_runs_across_three_containers() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let carol_node = stand.spawn("carol").await?;
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let carol = carol_node.create_identity().await?;
    let carol_work = carol_node.create_identity().await?;

    // The creator invites Bob, and Bob invites Carol.
    let family = alice_node.create_cell(alice).await?;
    let invite = alice_node.invite_to_cell(alice, family).await?;
    let joined: HeldCell = bob_node.join_cell(bob, invite).await?.json()?;
    assert_eq!(joined.cell, family);
    let invite = bob_node.invite_to_cell(bob, family).await?;
    let joined: HeldCell = carol_node.join_cell(carol, invite.clone()).await?.json()?;
    assert_eq!(joined.cell, family);
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
    let replayed = carol_node.join_cell(carol_work, invite).await?;
    assert_eq!(
        replayed.status,
        StatusCode::FORBIDDEN,
        "a consumed invite must be refused, got {}: {}",
        replayed.status,
        replayed.text()
    );
    let held: HeldCells = carol_node
        .get(&format!("/debug/identities/{carol_work}/cells"))
        .await?
        .json()?;
    assert!(
        held.cells.is_empty(),
        "a refused join must leave no cell behind: {held:?}"
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
    for act in [Act::Promote(carol), Act::Kick(bob)] {
        let refused = carol_node.cell_act(carol, family, act).await?;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "a plain member's {act:?} must be refused, got {}: {}",
            refused.status,
            refused.text()
        );
    }
    alice_node
        .cell_act(alice, family, Act::Promote(bob))
        .await?
        .ok()?;
    let promoted = [member(alice, true), member(bob, true), member(carol, false)];
    for (node, identity) in nodes {
        members_read(node, identity, family, &promoted).await?;
    }

    bob_node
        .cell_act(bob, family, Act::Kick(carol))
        .await?
        .ok()?;
    // Placed before Alice's replica holds the kick, a record would rightly
    // still be served to Carol.
    members_read(
        &alice_node,
        alice,
        family,
        &[member(alice, true), member(bob, true)],
    )
    .await?;
    let first = alice_node
        .place_record(alice, family, RecordKind::Claim, b"after the kick")
        .await?;
    let second = alice_node
        .place_record(alice, family, RecordKind::Claim, b"after the kick, again")
        .await?;
    record_reads(&bob_node, bob, family, first, b"after the kick").await?;
    record_reads(&bob_node, bob, family, second, b"after the kick, again").await?;

    // Denied (the kicked member): absent or refused, never read.
    let watched_until = tokio::time::Instant::now() + KICKED_WATCH;
    while tokio::time::Instant::now() < watched_until {
        for record in [first, second] {
            let answer = carol_node.get(&record_route(carol, family, record)).await?;
            assert!(
                matches!(answer.status, StatusCode::NOT_FOUND | StatusCode::CONFLICT),
                "the kicked member answered {} for {record:?}: {}",
                answer.status,
                answer.text()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Carol's device learns of its kick.
    cell_listed(&carol_node, carol, family, false).await?;
    let refused = carol_node
        .get(&format!("{}/members", cell_route(carol, family)))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "the kicked member's requests on the cell must be refused, got {}: {}",
        refused.status,
        refused.text()
    );

    Ok(())
}
