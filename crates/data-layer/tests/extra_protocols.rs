//! Externally supplied protocols on the node assembly: a test echo protocol
//! registered at spawn answers on its ALPN, dialed through the node's
//! exposed dial handle, while the built-in stack keeps syncing, and the
//! paired refusals hold —
//! a dial under an ALPN the node did not register fails without running any
//! handler, and an ALPN collision (built-in or duplicate) is refused at
//! spawn. The types come from `data-layer`'s re-exported extension surface,
//! the same way the future pairing handler will consume it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use data_layer::{
    AcceptError, AddrInfoOptions, AlpnTaken, Connection, ProtocolHandler, ShareMode, SyncNode,
    BUILT_IN_ALPNS,
};
use pdn_types::{EntryPath, NodeId};
use test_utils::{ids, wait_entry_is};

/// The test protocol's ALPN — deliberately not a built-in one.
const ECHO_ALPN: &[u8] = b"/pdn-test/echo/0";
/// An ALPN nobody registers in these tests.
const UNREGISTERED_ALPN: &[u8] = b"/pdn-test/unregistered/0";
/// The panicking test protocol's ALPN.
const PANIC_ALPN: &[u8] = b"/pdn-test/panic/0";
/// Payload ceiling for the echo streams.
const ECHO_LIMIT: usize = 1024;

/// Test-only echo protocol: accepts one bidirectional stream, sends back
/// what it read, and records how often it ran and who dialed.
#[derive(Debug, Clone, Default)]
struct EchoHandler {
    accepts: Arc<AtomicUsize>,
    remotes: Arc<Mutex<Vec<NodeId>>>,
}

impl ProtocolHandler for EchoHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.accepts.fetch_add(1, Ordering::SeqCst);
        self.remotes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(NodeId::from_bytes(*connection.remote_id().as_bytes()));
        let (mut send, mut recv) = connection.accept_bi().await?;
        let bytes = recv
            .read_to_end(ECHO_LIMIT)
            .await
            .map_err(AcceptError::from_err)?;
        send.write_all(&bytes)
            .await
            .map_err(AcceptError::from_err)?;
        send.finish()?;
        // Hold the connection until the dialer closes it, so the echoed
        // bytes are not cut off by dropping this side first.
        connection.closed().await;
        Ok(())
    }
}

/// Test-only handler that panics (out-of-bounds index) after reading the
/// dialer's bytes, recording that it ran. Stands in for a buggy extra
/// protocol whose panic must be contained rather than taking the node down.
#[derive(Debug, Clone, Default)]
struct PanickingHandler {
    ran: Arc<AtomicUsize>,
}

impl ProtocolHandler for PanickingHandler {
    // Panics the way a real handler bug would — an out-of-bounds index rather
    // than an explicit panic!() the lints deny. The index is allowed here
    // because the panic is the point; the guard must catch it all the same.
    #[allow(clippy::indexing_slicing)]
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        let (_send, mut recv) = connection.accept_bi().await?;
        let bytes = recv.read_to_end(ECHO_LIMIT).await.unwrap_or_default();
        let _boom = bytes[bytes.len()];
        Ok(())
    }
}

/// An extra protocol answers on its ALPN as a raw bidirectional stream, the
/// handler sees the dialer's node id as the remote identity, and the
/// built-in stack on the same endpoint keeps converging replicas.
#[tokio::test(flavor = "multi_thread")]
async fn extra_protocol_serves_alongside_sync() -> Result<()> {
    let echo = EchoHandler::default();
    let mut node_a =
        SyncNode::spawn_with_protocols(vec![(ECHO_ALPN.to_vec(), Box::new(echo.clone()))]).await?;
    let mut node_b = SyncNode::spawn().await?;

    // Dial A's extra protocol through B's dial handle; echo the bytes.
    let conn = node_b
        .dial_handle()
        .connect(node_a.dial_handle().addr(), ECHO_ALPN)
        .await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"ping").await?;
    send.finish()?;
    let echoed = recv.read_to_end(ECHO_LIMIT).await?;
    assert_eq!(echoed, b"ping", "echo returned different bytes");
    conn.close(0u32.into(), b"done");

    // The handler ran exactly once, and the remote identity it observed is
    // B's node id — the dial rode B's own endpoint.
    assert_eq!(echo.accepts.load(Ordering::SeqCst), 1);
    assert_eq!(
        echo.remotes.lock().expect("remotes lock").as_slice(),
        &[node_b.node_id()]
    );

    // The built-in stack is unaffected: the same pair converges a replica
    // over the ordinary ticket flow.
    let author = node_a.create_author().await?;
    node_a.create_namespace(ids::ALICE).await?;
    let name = EntryPath::new("profile/name")?;
    node_a.write(ids::ALICE, author, &name, b"Alice").await?;
    let ticket = node_a
        .share_ticket(
            ids::ALICE,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    node_b.import_namespace(ids::ALICE, ticket).await?;
    assert!(
        wait_entry_is(&node_b, ids::ALICE, &name, b"Alice").await?,
        "replica did not converge on the node serving an extra protocol"
    );

    node_a.shutdown().await?;
    node_b.shutdown().await?;
    Ok(())
}

/// A panic in an extra handler is contained: the panic is caught, that one
/// connection fails, and the node's built-in stack keeps syncing — the panic
/// does not tear the whole node down through iroh's accept loop.
#[tokio::test(flavor = "multi_thread")]
async fn panicking_extra_handler_does_not_take_down_the_node() -> Result<()> {
    let panicker = PanickingHandler::default();
    let mut node_a =
        SyncNode::spawn_with_protocols(vec![(PANIC_ALPN.to_vec(), Box::new(panicker.clone()))])
            .await?;
    let mut node_b = SyncNode::spawn().await?;

    // Dial the panicking protocol and drive a stream so its handler runs.
    // The handler panics after reading, so our side must see an error rather
    // than a clean response.
    let conn = node_b
        .dial_handle()
        .connect(node_a.dial_handle().addr(), PANIC_ALPN)
        .await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"trigger").await?;
    send.finish()?;
    let response = recv.read_to_end(ECHO_LIMIT).await;
    assert!(
        response.is_err(),
        "a panicking handler must not yield a clean response"
    );
    assert!(
        panicker.ran.load(Ordering::SeqCst) >= 1,
        "the handler should have run and panicked"
    );

    // The node survived: its built-in stack still converges a replica over
    // the ordinary ticket flow.
    let author = node_a.create_author().await?;
    node_a.create_namespace(ids::ALICE).await?;
    let name = EntryPath::new("profile/name")?;
    node_a.write(ids::ALICE, author, &name, b"Alice").await?;
    let ticket = node_a
        .share_ticket(
            ids::ALICE,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    node_b.import_namespace(ids::ALICE, ticket).await?;
    assert!(
        wait_entry_is(&node_b, ids::ALICE, &name, b"Alice").await?,
        "node stopped syncing after an extra handler panicked"
    );

    node_a.shutdown().await?;
    node_b.shutdown().await?;
    Ok(())
}

/// Paired refusal: a dial under an ALPN the node did not register fails —
/// no connection is established and the registered handler never runs.
#[tokio::test(flavor = "multi_thread")]
async fn unregistered_alpn_is_refused() -> Result<()> {
    let echo = EchoHandler::default();
    let node_a =
        SyncNode::spawn_with_protocols(vec![(ECHO_ALPN.to_vec(), Box::new(echo.clone()))]).await?;
    let node_b = SyncNode::spawn().await?;

    let refused = node_b
        .dial_handle()
        .connect(node_a.dial_handle().addr(), UNREGISTERED_ALPN)
        .await;
    assert!(
        refused.is_err(),
        "a dial under an unregistered ALPN must not establish a connection"
    );
    assert_eq!(
        echo.accepts.load(Ordering::SeqCst),
        0,
        "no handler may run for a refused ALPN"
    );

    node_a.shutdown().await?;
    node_b.shutdown().await?;
    Ok(())
}

/// Paired refusal at spawn: an extra protocol claiming a built-in ALPN —
/// any of blob transfer, gossip, document sync — or the same ALPN as
/// another extra fails the spawn with the typed collision error; no node
/// starts.
#[tokio::test(flavor = "multi_thread")]
async fn alpn_collisions_are_refused_at_spawn() -> Result<()> {
    for reserved in BUILT_IN_ALPNS {
        let err = SyncNode::spawn_with_protocols(vec![(
            reserved.to_vec(),
            Box::new(EchoHandler::default()),
        )])
        .await
        .expect_err("a built-in ALPN must be refused at spawn");
        let taken: &AlpnTaken = err.downcast_ref().expect("typed AlpnTaken error");
        assert_eq!(taken.alpn, reserved);
    }

    let err = SyncNode::spawn_with_protocols(vec![
        (ECHO_ALPN.to_vec(), Box::new(EchoHandler::default())),
        (ECHO_ALPN.to_vec(), Box::new(EchoHandler::default())),
    ])
    .await
    .expect_err("a duplicate extra ALPN must be refused at spawn");
    let taken: &AlpnTaken = err.downcast_ref().expect("typed AlpnTaken error");
    assert_eq!(taken.alpn, ECHO_ALPN);
    Ok(())
}

/// The dial handle carries the node's wire identity: its id equals the
/// node id the rest of the stack reports.
#[tokio::test(flavor = "multi_thread")]
async fn dial_handle_carries_the_nodes_wire_identity() -> Result<()> {
    let node = SyncNode::spawn().await?;
    assert_eq!(
        NodeId::from_bytes(*node.dial_handle().id().as_bytes()),
        node.node_id()
    );
    node.shutdown().await?;
    Ok(())
}
