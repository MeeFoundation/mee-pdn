//! Gossip discovery integration tests for Phase 1a.
//!
//! Run with: `cargo test -p mee-sync-iroh-willow --features gossip,test-utils`

use std::time::Duration;

use mee_sync_api::{AccessMode, SyncEngine};
use mee_sync_iroh_willow::gossip::GossipConfig;
use mee_sync_iroh_willow::test_helpers::{
    connect_via_invite, join_gossip_peers, wait_for_entry, wait_for_gossip_peer_count, TestNode,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Two nodes connected via invite exchange
/// `PeerAdvertisements` over gossip.
///
/// Setup:
///   - Spawn alice, bob with gossip enabled
///   - Connect alice -> bob via invite (establishes connectivity)
///   - Join gossip peers
///
/// Assert:
///   - Alice caches Bob's advertisement
///   - Bob caches Alice's advertisement
///   - Advertisements contain correct `peer_id`
#[tokio::test(flavor = "multi_thread")]
async fn two_node_gossip_exchange() {
    let alice = TestNode::spawn_with_gossip("alice").await.unwrap();
    let bob = TestNode::spawn_with_gossip("bob").await.unwrap();

    // Create namespaces so they have something to advertise
    let alice_sub = alice.core.subspace_id().await.unwrap();
    let _ns_alice = alice.core.create_namespace(&alice_sub).await.unwrap();

    let bob_sub = bob.core.subspace_id().await.unwrap();
    let _ns_bob = bob.core.create_namespace(&bob_sub).await.unwrap();

    // Connect via invite to establish iroh connectivity
    connect_via_invite(&alice.core, &bob.core, AccessMode::Read)
        .await
        .unwrap();

    // Join gossip mesh
    join_gossip_peers(&alice, &bob).await;

    // Trigger broadcasts
    let alice_mgr = alice.core.gossip_manager().expect("gossip manager");
    let bob_mgr = bob.core.gossip_manager().expect("gossip manager");

    alice_mgr.trigger_broadcast().await.unwrap();
    bob_mgr.trigger_broadcast().await.unwrap();

    // Wait for each to cache the other's advertisement
    let alice_peers = wait_for_gossip_peer_count(alice_mgr, 1, DEFAULT_TIMEOUT).await;
    let bob_peers = wait_for_gossip_peer_count(bob_mgr, 1, DEFAULT_TIMEOUT).await;

    // Verify alice cached bob's ad
    assert_eq!(alice_peers.len(), 1);
    assert_eq!(alice_peers[0].peer_id, *bob_sub.as_bytes());

    // Verify bob cached alice's ad
    assert_eq!(bob_peers.len(), 1);
    assert_eq!(bob_peers[0].peer_id, *alice_sub.as_bytes());
}

/// Node creates a new namespace, re-broadcasts advertisement,
/// peers see the updated version.
///
/// Setup:
///   - Spawn alice, bob, connected via invite
///   - Both join gossip topic
///   - Alice broadcasts initial ad
///   - Bob caches it
///
/// Action:
///   - Alice creates a second namespace
///   - Alice triggers re-broadcast
///
/// Assert:
///   - Bob's cached ad for alice has version > initial
///   - Bob's cached ad includes the new namespace
#[tokio::test(flavor = "multi_thread")]
async fn advertisement_rebroadcast_on_namespace_change() {
    let alice = TestNode::spawn_with_gossip("alice").await.unwrap();
    let bob = TestNode::spawn_with_gossip("bob").await.unwrap();

    let alice_sub = alice.core.subspace_id().await.unwrap();
    let _ns1 = alice.core.create_namespace(&alice_sub).await.unwrap();

    // Connect and join gossip
    connect_via_invite(&alice.core, &bob.core, AccessMode::Read)
        .await
        .unwrap();

    join_gossip_peers(&alice, &bob).await;

    let alice_mgr = alice.core.gossip_manager().expect("gossip");
    let bob_mgr = bob.core.gossip_manager().expect("gossip");

    // Initial broadcast
    alice_mgr.trigger_broadcast().await.unwrap();

    // Wait for bob to cache alice's first ad
    let first_peers = wait_for_gossip_peer_count(bob_mgr, 1, DEFAULT_TIMEOUT).await;
    let first_version = first_peers[0].version;
    let first_ns_count = first_peers[0].namespace_ids.len();

    // Alice creates a second namespace
    let _ns2 = alice.core.create_namespace(&alice_sub).await.unwrap();

    // Alice re-broadcasts
    alice_mgr.trigger_broadcast().await.unwrap();

    // Wait for bob to see updated ad (higher version)
    let deadline = tokio::time::Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if let Ok(peers) = bob_mgr.cached_peers().await {
            if let Some(p) = peers.first() {
                if p.version > first_version {
                    // Version bumped
                    assert!(
                        p.namespace_ids.len() > first_ns_count,
                        "expected more namespaces"
                    );
                    break;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for updated ad"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stale advertisements are evicted from the peer cache.
///
/// Setup:
///   - Spawn alice (short rebroadcast to ensure delivery)
///   - Spawn bob (short eviction: 2s)
///   - Connect, join gossip
///   - Bob caches alice's ad
///
/// Action:
///   - Drop alice (stops all broadcasts)
///   - Wait for eviction threshold
///
/// Assert:
///   - Alice's ad is evicted from bob's cache
#[tokio::test(flavor = "multi_thread")]
async fn stale_advertisement_eviction() {
    // Bob: short eviction so stale entries are removed quickly
    let bob_config = GossipConfig {
        rebroadcast_interval: Duration::from_secs(3600),
        staleness_threshold: Duration::from_secs(2),
        eviction_threshold: Duration::from_secs(2),
        bootstrap_peers: vec![],
    };
    let bob = TestNode::spawn_with_gossip_config("bob", bob_config)
        .await
        .unwrap();

    let bob_mgr = bob.core.gossip_manager().expect("gossip");

    // Scope alice so we can drop her
    {
        let alice = TestNode::spawn_with_gossip("alice").await.unwrap();

        let alice_sub = alice.core.subspace_id().await.unwrap();
        let _ns = alice.core.create_namespace(&alice_sub).await.unwrap();

        // Connect and join gossip
        connect_via_invite(&alice.core, &bob.core, AccessMode::Read)
            .await
            .unwrap();

        join_gossip_peers(&alice, &bob).await;

        // Trigger alice's broadcast
        alice
            .core
            .gossip_manager()
            .expect("gossip")
            .trigger_broadcast()
            .await
            .unwrap();

        // Wait for bob to cache it
        wait_for_gossip_peer_count(bob_mgr, 1, DEFAULT_TIMEOUT).await;

        // alice is dropped here — stops all broadcasts
    }

    // Wait for eviction (bob's threshold is 2s)
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify alice's ad was evicted
    let deadline = tokio::time::Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if let Ok(count) = bob_mgr.peer_count().await {
            if count == 0 {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for eviction"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Three-node transitive discovery: Alice finds Charlie
/// through Bob via gossip.
///
/// Setup:
///   - alice <-> bob: connected via invite
///   - bob <-> charlie: connected via invite
///   - Charlie delegates cap for `namespace_charlie` to alice
///     (out-of-band, with empty nodes so no direct connection)
///   - All join gossip mesh
///
/// Assert:
///   - Charlie inserts entry, alice sees it via
///     gossip-triggered auto-connect
#[tokio::test(flavor = "multi_thread")]
async fn three_node_transitive_discovery() {
    let alice = TestNode::spawn_with_gossip("alice").await.unwrap();
    let bob = TestNode::spawn_with_gossip("bob").await.unwrap();
    let charlie = TestNode::spawn_with_gossip("charlie").await.unwrap();

    // alice <-> bob connectivity
    connect_via_invite(&alice.core, &bob.core, AccessMode::Read)
        .await
        .unwrap();

    // bob <-> charlie connectivity
    connect_via_invite(&bob.core, &charlie.core, AccessMode::Read)
        .await
        .unwrap();

    // Charlie creates a namespace
    let charlie_sub = charlie.core.subspace_id().await.unwrap();
    let ns_charlie = charlie.core.create_namespace(&charlie_sub).await.unwrap();

    // Give alice capability for charlie's namespace
    // (out-of-band: share with empty nodes so no direct
    // connection is established)
    let alice_sub = alice.core.subspace_id().await.unwrap();
    let mut ticket = charlie
        .core
        .share(&ns_charlie, &alice_sub, AccessMode::Read)
        .await
        .unwrap();
    ticket.nodes.clear(); // No addresses — alice can't connect directly
    alice
        .core
        .import_and_sync(ticket, mee_sync_api::SyncMode::Continuous)
        .await
        .unwrap();

    // Join gossip: alice <-> bob <-> charlie
    join_gossip_peers(&alice, &bob).await;
    join_gossip_peers(&bob, &charlie).await;

    // Trigger broadcasts on all nodes
    let alice_mgr = alice.core.gossip_manager().expect("gossip");
    let bob_mgr = bob.core.gossip_manager().expect("gossip");
    let charlie_mgr = charlie.core.gossip_manager().expect("gossip");

    alice_mgr.trigger_broadcast().await.unwrap();
    bob_mgr.trigger_broadcast().await.unwrap();
    charlie_mgr.trigger_broadcast().await.unwrap();

    // Charlie inserts an entry
    let path = mee_sync_api::EntryPath::new("msgs/hello").unwrap();
    charlie
        .core
        .insert(&ns_charlie, &path, b"from charlie")
        .await
        .unwrap();

    // Alice should eventually see the entry via
    // gossip-triggered auto-connect
    let entry = wait_for_entry(&alice.core, &ns_charlie, "msgs/hello", DEFAULT_TIMEOUT).await;
    assert_eq!(entry.namespace, ns_charlie);
}

/// Full pipeline: gossip match -> connect -> sync -> data.
///
/// Same as `three_node_transitive_discovery` but focuses on
/// verifying the complete pipeline from gossip advertisement
/// to data replication.
#[tokio::test(flavor = "multi_thread")]
async fn gossip_match_to_sync_pipeline() {
    let alice = TestNode::spawn_with_gossip("alice").await.unwrap();
    let bob = TestNode::spawn_with_gossip("bob").await.unwrap();
    let charlie = TestNode::spawn_with_gossip("charlie").await.unwrap();

    // Establish connectivity: alice <-> bob <-> charlie
    connect_via_invite(&alice.core, &bob.core, AccessMode::Read)
        .await
        .unwrap();
    connect_via_invite(&bob.core, &charlie.core, AccessMode::Read)
        .await
        .unwrap();

    // Charlie creates namespace and inserts data FIRST
    let charlie_sub = charlie.core.subspace_id().await.unwrap();
    let ns_charlie = charlie.core.create_namespace(&charlie_sub).await.unwrap();
    let path = mee_sync_api::EntryPath::new("data/secret").unwrap();
    charlie
        .core
        .insert(&ns_charlie, &path, b"charlie-data")
        .await
        .unwrap();

    // Delegate capability to alice (out-of-band, no addresses)
    let alice_sub = alice.core.subspace_id().await.unwrap();
    let mut ticket = charlie
        .core
        .share(&ns_charlie, &alice_sub, AccessMode::Read)
        .await
        .unwrap();
    ticket.nodes.clear();
    alice
        .core
        .import_and_sync(ticket, mee_sync_api::SyncMode::Continuous)
        .await
        .unwrap();

    // Join gossip mesh
    join_gossip_peers(&alice, &bob).await;
    join_gossip_peers(&bob, &charlie).await;

    // Trigger all broadcasts
    for node in [&alice, &bob, &charlie] {
        node.core
            .gossip_manager()
            .expect("gossip")
            .trigger_broadcast()
            .await
            .unwrap();
    }

    // Alice discovers charlie via gossip, auto-connects,
    // and syncs the data
    let entry = wait_for_entry(&alice.core, &ns_charlie, "data/secret", DEFAULT_TIMEOUT).await;
    assert_eq!(entry.namespace, ns_charlie);
}
