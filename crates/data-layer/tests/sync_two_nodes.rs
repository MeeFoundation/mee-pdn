//! Capability-gated end-to-end sync over iroh-docs.
//!
//! Two in-process nodes built from `data-layer`. Bob's node accepts an
//! incoming entry only if the issuer of the entry's namespace is in his
//! live `Connections` set — the simplified, single-link form of a capability
//! chain, enforced by the `ConnectionsPolicy` ingest gate inside the local
//! iroh-docs variant.

use std::time::{Duration, Instant};

use anyhow::Result;
use data_layer::{AddrInfoOptions, Connections, ConnectionsPolicy, ShareMode, SyncNode};
use pdn_types::{EntryPath, NamespaceId, PdnId};

/// Poll until `path` is present on `node`, or return `None` once `timeout`
/// elapses.
async fn wait_for_entry(
    node: &SyncNode,
    namespace: &NamespaceId,
    path: &EntryPath,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = node.read(namespace, path).await? {
            return Ok(Some(value));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn capability_gated_sync() -> Result<()> {
    // PDN identities.
    let carol = PdnId::from_bytes([0xca; 32]);
    let alice = PdnId::from_bytes([0xa1; 32]);

    // Each node runs a connections-gated ingest policy; the `Connections`
    // handles stay with the test so the sets can be mutated at runtime.
    let alice_connections = Connections::new();
    let bob_connections = Connections::new();
    let mut alice_node = SyncNode::spawn(ConnectionsPolicy::new(alice_connections.clone())).await?;
    let mut bob_node = SyncNode::spawn(ConnectionsPolicy::new(bob_connections.clone())).await?;

    // Namespace about Carol, issued by Alice (the sole writer/owner).
    let namespace = NamespaceId::new(carol, alice);
    assert_eq!(namespace.issued_by, alice);

    let alice_author = alice_node.create_author().await?;
    alice_node.create_namespace(namespace).await?;

    let k1 = EntryPath::new("k1")?;
    let k2 = EntryPath::new("k2")?;
    let k3 = EntryPath::new("k3")?;

    // Alice writes k1 before Bob connects: this entry reaches Bob via the
    // initial set-reconciliation path.
    alice_node
        .write(&namespace, alice_author, &k1, b"v1")
        .await?;

    let ticket = alice_node
        .share_ticket(
            &namespace,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    bob_node.import_namespace(namespace, ticket).await?;

    // Step 1 — Alice is NOT among Bob's connections: the gate drops k1.
    let got = wait_for_entry(&bob_node, &namespace, &k1, Duration::from_secs(3)).await?;
    assert!(
        got.is_none(),
        "k1 synced even though Alice is not a connection"
    );

    // Step 2 — add Alice as a connection; a fresh write (k2) must now sync
    // (live-gossip path -> insert_entry -> validate_entry -> accept).
    bob_connections.insert(alice);
    alice_node
        .write(&namespace, alice_author, &k2, b"v2")
        .await?;
    let got = wait_for_entry(&bob_node, &namespace, &k2, Duration::from_secs(15)).await?;
    assert_eq!(
        got.as_deref(),
        Some(b"v2".as_ref()),
        "k2 did not sync after connecting Alice"
    );

    // Step 3 — revoke Alice (remove from connections); a fresh write (k3) is
    // rejected again, symmetric to step 1.
    bob_connections.remove(&alice);
    alice_node
        .write(&namespace, alice_author, &k3, b"v3")
        .await?;
    let got = wait_for_entry(&bob_node, &namespace, &k3, Duration::from_secs(3)).await?;
    assert!(got.is_none(), "k3 synced even though Alice was revoked");

    alice_node.shutdown().await?;
    bob_node.shutdown().await?;
    Ok(())
}
