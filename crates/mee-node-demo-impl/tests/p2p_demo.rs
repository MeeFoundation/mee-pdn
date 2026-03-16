//! Integration tests replacing the `just p2p-demo` bash script.
//!
//! Two `DemoNodes` connected via the invite/connect flow, with data
//! replication verified through Willow sync.

use std::time::Duration;

use mee_node_api::{Contact, DataService, Node, SyncService, TrustService};
use mee_node_demo_impl::DemoNode;
use mee_sync_api::AccessMode;
use mee_sync_iroh_willow::DiscoveryConfig;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Poll `DataService::list()` until an entry with the given key appears.
async fn wait_for_entry_via_data(data: &impl DataService, expected_key: &str, max_wait: Duration) {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if let Ok(entries) = data.list("").await {
            if entries.iter().any(|e| e.key == expected_key) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for entry '{expected_key}' after {max_wait:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Two nodes: invite -> connect -> insert -> replicate.
///
/// Flow:
/// 1. Bob creates an invite (contains his AID, subspace, address)
/// 2. Alice connects using Bob's invite — this shares Alice's namespace
///    with Bob via `connect_and_share`
/// 3. Alice inserts data via `DataService`
/// 4. Bob sees it because he imported Alice's namespace during the
///    connect handshake
#[tokio::test(flavor = "multi_thread")]
async fn invite_connect_and_replicate() {
    tokio::time::timeout(TIMEOUT, async {
        let alice = DemoNode::spawn(DiscoveryConfig::test())
            .await
            .expect("alice spawn");
        let bob = DemoNode::spawn(DiscoveryConfig::test())
            .await
            .expect("bob spawn");

        // Bob creates an invite
        let invite = bob.trust().create_invite().await.expect("create invite");

        // Alice remembers the invite and connects
        alice.trust().remember_invite(invite.clone());
        alice.trust().add_contact(Contact {
            aid: invite.inviter_aid,
            alias: None,
        });
        let node_hint = invite
            .node_hints
            .first()
            .expect("test invite has node_hints");
        alice
            .sync()
            .connect_to_peer(&invite.subspace_id, node_hint, AccessMode::Read)
            .await
            .expect("connect to bob");

        // Alice inserts data via DataService
        alice
            .data()
            .set("msgs/hello", "from alice")
            .await
            .expect("insert");

        // Bob should see Alice's entry via continuous sync
        wait_for_entry_via_data(bob.data(), "msgs/hello", Duration::from_secs(10)).await;
    })
    .await
    .expect("test timed out");
}
