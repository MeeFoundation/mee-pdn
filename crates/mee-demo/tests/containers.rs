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

use common::{create_network, remove_network, wait_for_entry, wait_for_gossip_peers, MeeNode};

const NETWORK: &str = "mee-test-net";
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Two nodes: invite -> connect -> insert -> replicate -> stop -> restart -> reconnect.
///
/// Verifies:
/// 1. Basic P2P replication via HTTP API
/// 2. Node stop/start lifecycle
/// 3. Reconnection after restart (state is lost, fresh invite needed)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + pre-built mee-demo:dev image"]
async fn two_node_stop_restart() {
    Box::pin(run_with_network(NETWORK, two_node_stop_restart_inner())).await;
}

/// Run an async test body with a Docker network, guaranteeing cleanup on
/// both success and panic.
async fn run_with_network<F: Future<Output = ()>>(network: &str, body: F) {
    create_network(network).await;
    // Pin the future so we can poll it inside catch_unwind.
    let body = Pin::from(Box::new(body));
    let result = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    remove_network(network).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

async fn two_node_stop_restart_inner() {
    // --- Phase 1: spin up two nodes and verify replication ---

    let alice = MeeNode::spawn("alice", NETWORK).await;
    let bob = MeeNode::spawn("bob", NETWORK).await;

    assert!(alice.is_alive().await, "alice should be alive");
    assert!(bob.is_alive().await, "bob should be alive");

    // Bob creates an invite, Alice connects
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;

    // Alice inserts data
    alice.insert("msgs/hello", "hello from alice").await;

    // Bob should see it via Willow replication
    wait_for_entry(&bob, "msgs/hello", SYNC_TIMEOUT).await;

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
    alice.insert("msgs/post-restart", "back online").await;

    // Bob should receive the new entry
    wait_for_entry(&bob, "msgs/post-restart", SYNC_TIMEOUT).await;
}

// ---- Gossip discovery tests ------------------------------------------------
//
// All gossip tests rely on the organic rebroadcast timer
// (MEE_GOSSIP_REBROADCAST_SECS=3) instead of manual broadcast triggers.

/// Three-node transitive sync: Alice discovers Charlie through Bob
/// via gossip, with NO prior direct connection to Charlie.
///
/// Gossip mesh is wired automatically by the invite/connect flow.
/// Rebroadcast timer propagates advertisements organically.
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
    let alice = MeeNode::spawn_with_gossip("alice", "mee-gossip-trans").await;
    let bob = MeeNode::spawn_with_gossip("bob", "mee-gossip-trans").await;
    let charlie = MeeNode::spawn_with_gossip("charlie", "mee-gossip-trans").await;

    // Establish connectivity via invites (auto-wires gossip):
    // Alice <-> Bob
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;
    let alice_invite = alice.get_invite().await;
    bob.connect(&alice_invite).await;

    // Bob <-> Charlie
    let charlie_invite = charlie.get_invite().await;
    bob.connect(&charlie_invite).await;
    let bob_invite2 = bob.get_invite().await;
    charlie.connect(&bob_invite2).await;

    // Charlie creates a ticket for Alice's subspace (out-of-band capability
    // delegation). Strip addresses so Alice can't connect directly.
    let alice_sub = alice.subspace_id().await;
    let mut ticket = charlie.create_ticket(&alice_sub).await;
    ticket["node_hints"] = serde_json::json!([]);

    // Alice imports the address-less ticket — she now holds a capability
    // for Charlie's namespace but has no address for Charlie.
    alice.import_ticket(&ticket).await;

    // Charlie inserts data
    charlie.insert("msgs/hello", "from charlie").await;

    // Alice should discover Charlie via gossip (relayed through Bob),
    // see namespace overlap, auto-connect, and sync data.
    wait_for_entry(&alice, "msgs/hello", SYNC_TIMEOUT).await;
}

/// After invite/connect, gossip auto-connects symmetrically.
///
/// Both sides see namespace overlap from the invite exchange.
/// The rebroadcast timer propagates advertisements, and gossip
/// auto-connect fires on both sides.
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
    let alice = MeeNode::spawn_with_gossip("alice", "mee-gossip-sym").await;
    let bob = MeeNode::spawn_with_gossip("bob", "mee-gossip-sym").await;

    // Alice connects to Bob via invite (auto-wires gossip)
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;

    // Wait for organic rebroadcast to propagate ads
    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;
    wait_for_gossip_peers(&bob, 1, SYNC_TIMEOUT).await;

    // Both sides see overlap → both auto-connect
    let alice_peers = alice.gossip_peers().await;
    assert_eq!(alice_peers.len(), 1);
    assert_eq!(
        alice_peers[0]["connected"].as_bool(),
        Some(true),
        "alice sees her namespace in bob's ad → auto-connect"
    );

    let bob_peers = bob.gossip_peers().await;
    assert_eq!(bob_peers.len(), 1);
    assert_eq!(
        bob_peers[0]["connected"].as_bool(),
        Some(true),
        "bob imported alice's namespace → auto-connect"
    );
}

/// Two nodes with NO shared namespaces see each other via gossip
/// but do NOT auto-connect.
///
/// Alice and Bob each connect to a Bridge node via invite, putting
/// them in the same gossip mesh. But Alice and Bob have never
/// exchanged invites with each other — no shared namespace caps.
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
    let alice = MeeNode::spawn_with_gossip("alice", "mee-gossip-noop").await;
    let bob = MeeNode::spawn_with_gossip("bob", "mee-gossip-noop").await;
    let bridge = MeeNode::spawn_with_gossip("bridge", "mee-gossip-noop").await;

    // Alice↔Bridge and Bob↔Bridge via invite (auto-wires gossip).
    // Alice and Bob are NOT directly connected — no shared namespaces.
    let bridge_invite1 = bridge.get_invite().await;
    alice.connect(&bridge_invite1).await;
    let bridge_invite2 = bridge.get_invite().await;
    bob.connect(&bridge_invite2).await;

    // Wait for organic rebroadcast to propagate ads through Bridge
    wait_for_gossip_peers(&alice, 2, SYNC_TIMEOUT).await;
    wait_for_gossip_peers(&bob, 2, SYNC_TIMEOUT).await;

    // Find Bob in Alice's peer cache
    let bob_sub = bob.subspace_id().await;
    let alice_peers = alice.gossip_peers().await;
    let bob_in_alice = alice_peers
        .iter()
        .find(|p| p["peer_id"].as_str() == Some(&bob_sub));
    assert!(
        bob_in_alice.is_some(),
        "alice should cache bob's ad (via bridge)"
    );
    assert_eq!(
        bob_in_alice.expect("checked above")["connected"].as_bool(),
        Some(false),
        "alice has no namespace overlap with bob → no auto-connect"
    );
}

/// After a node restarts, gossip re-discovers it with a new identity.
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
    let alice = MeeNode::spawn_with_gossip("alice", "mee-gossip-restart").await;
    let bob = MeeNode::spawn_with_gossip("bob", "mee-gossip-restart").await;

    // Initial gossip discovery via invite
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;
    let alice_invite = alice.get_invite().await;
    bob.connect(&alice_invite).await;

    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;

    // Stop Bob — Alice should eventually evict stale ad
    bob.stop().await;

    // Wait for eviction (gossip eviction threshold is 3s in test config)
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

    // Restart Bob with fresh identity
    bob.start().await;

    // Re-connect via invite (auto-wires gossip)
    let new_bob_invite = bob.get_invite().await;
    alice.connect(&new_bob_invite).await;
    let new_alice_invite = alice.get_invite().await;
    bob.connect(&new_alice_invite).await;

    // Alice should re-discover Bob via organic rebroadcast
    wait_for_gossip_peers(&alice, 1, SYNC_TIMEOUT).await;
}

/// Deferred gossip-based invite discovery: Alice receives an invite
/// from Charlie with empty `node_hints`. The connect returns "pending".
/// Gossip discovers Charlie through Bob and auto-connects.
///
/// This is the phone->PC scenario: Charlie invites Alice on his phone,
/// turns it off, turns on his PC. Gossip finds his PC and syncs.
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
    let alice = MeeNode::spawn_with_gossip("alice", "mee-gossip-defer").await;
    let bob = MeeNode::spawn_with_gossip("bob", "mee-gossip-defer").await;
    let charlie = MeeNode::spawn_with_gossip("charlie", "mee-gossip-defer").await;

    // Establish gossip mesh: Alice↔Bob, Bob↔Charlie (via invites)
    let bob_invite = bob.get_invite().await;
    alice.connect(&bob_invite).await;
    let alice_invite = alice.get_invite().await;
    bob.connect(&alice_invite).await;

    let charlie_invite_for_bob = charlie.get_invite().await;
    bob.connect(&charlie_invite_for_bob).await;
    let bob_invite_for_charlie = bob.get_invite().await;
    charlie.connect(&bob_invite_for_charlie).await;

    // Charlie creates an invite for Alice — but we strip node_hints
    // to simulate the "phone turned off" scenario.
    let mut charlie_invite = charlie.get_invite().await;
    charlie_invite["node_hints"] = serde_json::json!([]);

    // Alice connects with the empty invite — should return "pending"
    let status = alice.connect_status(&charlie_invite).await;
    assert_eq!(status, "pending", "empty node_hints → deferred discovery");

    // Alice also needs Charlie's namespace capability to match the
    // gossip advertisement. Import an address-less ticket.
    let alice_sub = alice.subspace_id().await;
    let mut ticket = charlie.create_ticket(&alice_sub).await;
    ticket["node_hints"] = serde_json::json!([]);
    alice.import_ticket(&ticket).await;

    // Charlie inserts data
    charlie.insert("msgs/deferred", "found via gossip").await;

    // Alice should discover Charlie via pending invite matching
    // in the gossip event loop, auto-connect, and sync data.
    // No manual broadcast needed — organic rebroadcast timer handles it.
    wait_for_entry(&alice, "msgs/deferred", SYNC_TIMEOUT).await;
}
