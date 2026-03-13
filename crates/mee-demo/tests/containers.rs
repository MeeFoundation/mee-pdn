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

use common::{create_network, remove_network, wait_for_entry, MeeNode};

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
