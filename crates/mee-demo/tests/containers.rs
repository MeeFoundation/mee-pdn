#![allow(clippy::indexing_slicing)]
//! Integration tests using Docker containers (testcontainers).
//!
//! Prerequisites:
//! - Docker running
//! - Image built: `just build-image`
//!
//! Run with: `just integration-tests`

mod common;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::FutureExt;

use common::{
    bidirectional_connect, create_network, remove_network, wait_for_entry, wait_for_gossip_peers,
    MeeNode,
};

const NETWORK: &str = "mee-test-net";
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Two nodes: invite -> connect -> insert -> replicate -> stop -> restart -> reconnect.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn two_node_stop_restart() {
    Box::pin(run_with_network(NETWORK, two_node_stop_restart_inner())).await;
}

async fn run_with_network<F: Future<Output = ()>>(network: &str, body: F) {
    create_network(network).await;
    let body = Pin::from(Box::new(body));
    let result = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    remove_network(network).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

async fn two_node_stop_restart_inner() {
    let alice = MeeNode::spawn("alice", NETWORK).await;
    let bob = MeeNode::spawn("bob", NETWORK).await;

    assert!(alice.is_alive().await, "alice should be alive");
    assert!(bob.is_alive().await, "bob should be alive");

    let alice_ns = alice.home_namespace().await;

    // Bob creates an invite, Alice connects
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;

    // Alice inserts data into her namespace
    alice
        .insert(&alice_ns, "msgs/hello", "hello from alice")
        .await;

    // Bob should see it via Willow replication (in Alice's namespace)
    wait_for_entry(&bob, &alice_ns, "msgs/hello", SYNC_TIMEOUT).await;

    // --- Phase 2: stop Bob, verify Alice is still alive ---
    bob.stop().await;
    assert!(!bob.is_alive().await, "bob should be stopped");
    assert!(alice.is_alive().await, "alice should still be alive");

    // --- Phase 3: restart Bob, reconnect, verify sync works again ---
    bob.start().await;
    assert!(bob.is_alive().await, "bob should be alive after restart");

    // Bob has fresh state — new invite/connect cycle needed
    let new_bob_invite = bob.get_invite().await;
    alice.connect(&new_bob_invite).await;

    // Insert new data on Alice
    alice
        .insert(&alice_ns, "msgs/post-restart", "back online")
        .await;

    // Bob should receive the new entry (in Alice's namespace)
    wait_for_entry(&bob, &alice_ns, "msgs/post-restart", SYNC_TIMEOUT).await;
}

// ---- Gossip discovery tests ------------------------------------------------

/// Three-node transitive sync via gossip.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn gossip_transitive_sync() {
    Box::pin(run_with_network(
        "mee-gossip-trans",
        gossip_transitive_sync_inner(),
    ))
    .await;
}

async fn gossip_transitive_sync_inner() {
    let alice = MeeNode::spawn("alice", "mee-gossip-trans").await;
    let bob = MeeNode::spawn("bob", "mee-gossip-trans").await;
    let charlie = MeeNode::spawn("charlie", "mee-gossip-trans").await;

    let charlie_ns = charlie.home_namespace().await;

    // Establish connectivity via invites (auto-wires gossip)
    bidirectional_connect(&alice, &bob).await;
    bidirectional_connect(&bob, &charlie).await;

    // Charlie creates a ticket for Alice's subspace — strip addresses
    let alice_sub = alice.subspace_id().await;
    let mut ticket = charlie.create_ticket(&alice_sub).await;
    ticket["node_hints"] = serde_json::json!([]);
    alice.import_ticket(&ticket).await;

    // Charlie inserts data
    charlie
        .insert(&charlie_ns, "msgs/hello", "from charlie")
        .await;

    // Alice discovers Charlie via gossip, auto-connects, syncs
    wait_for_entry(&alice, &charlie_ns, "msgs/hello", SYNC_TIMEOUT).await;
}

/// After invite/connect, gossip auto-connects symmetrically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn gossip_symmetric_connect_after_invite() {
    Box::pin(run_with_network(
        "mee-gossip-sym",
        gossip_symmetric_connect_after_invite_inner(),
    ))
    .await;
}

async fn gossip_symmetric_connect_after_invite_inner() {
    let alice = MeeNode::spawn("alice", "mee-gossip-sym").await;
    let bob = MeeNode::spawn("bob", "mee-gossip-sym").await;

    let bob_sub = bob.subspace_id().await;
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;

    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;
    wait_for_gossip_peers(&bob, 1, SYNC_TIMEOUT).await;

    // Alice should have an audit connection entry for Bob
    let alice_conns = alice.connections().await;
    assert!(
        alice_conns.contains(&bob_sub),
        "alice audit log should contain bob's subspace_id"
    );
}

/// No shared namespaces → no auto-connect (via bridge node).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn gossip_no_overlap_no_connect() {
    Box::pin(run_with_network(
        "mee-gossip-noop",
        gossip_no_overlap_no_connect_inner(),
    ))
    .await;
}

async fn gossip_no_overlap_no_connect_inner() {
    let alice = MeeNode::spawn("alice", "mee-gossip-noop").await;
    let bob = MeeNode::spawn("bob", "mee-gossip-noop").await;
    let bridge = MeeNode::spawn("bridge", "mee-gossip-noop").await;

    let bridge_invite1 = bridge.get_invite().await;
    alice.connect(&bridge_invite1).await;
    let bridge_invite2 = bridge.get_invite().await;
    bob.connect(&bridge_invite2).await;

    wait_for_gossip_peers(&alice, 2, SYNC_TIMEOUT).await;
    wait_for_gossip_peers(&bob, 2, SYNC_TIMEOUT).await;

    let bob_sub = bob.subspace_id().await;
    let alice_conns = alice.connections().await;
    assert!(
        !alice_conns.contains(&bob_sub),
        "no namespace overlap → no audit connection entry"
    );
}

/// Node restart + gossip re-discovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn gossip_restart_rediscovery() {
    Box::pin(run_with_network(
        "mee-gossip-restart",
        gossip_restart_rediscovery_inner(),
    ))
    .await;
}

async fn gossip_restart_rediscovery_inner() {
    let alice = MeeNode::spawn("alice", "mee-gossip-restart").await;
    let bob = MeeNode::spawn("bob", "mee-gossip-restart").await;

    bidirectional_connect(&alice, &bob).await;
    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;

    bob.stop().await;

    let deadline = tokio::time::Instant::now() + SYNC_TIMEOUT;
    loop {
        let peers = alice.gossip_peers().await;
        if peers.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for eviction"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    bob.start().await;

    bidirectional_connect(&alice, &bob).await;
    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;
}

/// Deferred gossip-based invite discovery (phone->PC scenario).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn gossip_deferred_invite_discovery() {
    Box::pin(run_with_network(
        "mee-gossip-defer",
        gossip_deferred_invite_discovery_inner(),
    ))
    .await;
}

async fn gossip_deferred_invite_discovery_inner() {
    let alice = MeeNode::spawn("alice", "mee-gossip-defer").await;
    let bob = MeeNode::spawn("bob", "mee-gossip-defer").await;
    let charlie = MeeNode::spawn("charlie", "mee-gossip-defer").await;

    let charlie_ns = charlie.home_namespace().await;

    // Gossip mesh: Alice↔Bob, Bob↔Charlie
    bidirectional_connect(&alice, &bob).await;
    bidirectional_connect(&bob, &charlie).await;

    // Charlie invite with empty hints → deferred discovery
    let mut charlie_invite = charlie.get_invite().await;
    charlie_invite["node_hints"] = serde_json::json!([]);

    let status = alice.connect_status(&charlie_invite).await;
    assert_eq!(status, "pending", "empty node_hints → deferred");

    // Import capability for Charlie's namespace (address-less)
    let alice_sub = alice.subspace_id().await;
    let mut ticket = charlie.create_ticket(&alice_sub).await;
    ticket["node_hints"] = serde_json::json!([]);
    alice.import_ticket(&ticket).await;

    // Charlie inserts data
    charlie
        .insert(&charlie_ns, "msgs/deferred", "found via gossip")
        .await;

    // Alice discovers Charlie via gossip
    wait_for_entry(&alice, &charlie_ns, "msgs/deferred", SYNC_TIMEOUT).await;
}
