//! The refusal and containment edges of externally supplied protocols: an
//! unregistered ALPN, an ALPN collision at spawn, a panicking handler. The
//! happy path is exercised by the real consumer, pdn-node's pairing
//! protocol; these edges are what it never triggers.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, PoisonError,
};

use anyhow::Result;
use data_layer::{
    AcceptError, AddrInfoOptions, AlpnTaken, Connection, ProtocolHandler, ShareMode, SpawnOptions,
    SyncNode, BUILT_IN_ALPNS,
};
use pdn_types::{EntryPath, NodeId};
use test_utils::{host_identity, ids, join_identity, memory_node, wait_entry_is};

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

/// Panics (out-of-bounds index) after reading the dialer's bytes.
#[derive(Debug, Clone, Default)]
struct PanickingHandler {
    ran: Arc<AtomicUsize>,
}

impl ProtocolHandler for PanickingHandler {
    // An out-of-bounds index rather than the `panic!()` the lints deny.
    #[allow(clippy::indexing_slicing)]
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        let (_send, mut recv) = connection.accept_bi().await?;
        let bytes = recv.read_to_end(ECHO_LIMIT).await.unwrap_or_default();
        let _boom = bytes[bytes.len()];
        Ok(())
    }
}

/// A panic in an extra handler is contained: that one connection fails and
/// the node's built-in stack keeps syncing.
#[tokio::test(flavor = "multi_thread")]
async fn panicking_extra_handler_does_not_take_down_the_node() -> Result<()> {
    let panicker = PanickingHandler::default();
    let node_a = SyncNode::spawn_with(
        vec![(PANIC_ALPN.to_vec(), Box::new(panicker.clone()))],
        SpawnOptions::memory(),
    )
    .await?;
    let node_b = memory_node().await?;

    // Patient: a process's first accepted-ALPN handshake can stall on this
    // machine, and this dial is the binary's first.
    let deadline = std::time::Instant::now() + test_utils::TIMEOUT;
    let conn = loop {
        match node_b
            .dial_handle()
            .connect(node_a.dial_handle().addr(), PANIC_ALPN)
            .await
        {
            Ok(conn) => break conn,
            Err(err) if std::time::Instant::now() > deadline => return Err(err),
            Err(_transient) => {}
        }
    };
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"trigger").await?;
    send.finish()?;
    // Stream error or clean empty end-of-stream is a teardown race (the
    // unwind drops the handler's `SendStream` before the connection), not
    // the containment property.
    let response = recv.read_to_end(ECHO_LIMIT).await;
    let payload = response.as_deref().unwrap_or_default();
    assert!(
        payload.is_empty(),
        "a panicking handler must not yield a payload, got {payload:?}"
    );
    assert!(
        panicker.ran.load(Ordering::SeqCst) >= 1,
        "the handler should have run and panicked"
    );

    // The node survived: a device of Alice still catches up her data
    // namespace from it.
    let directory = host_identity(&node_a, ids::ALICE).await?;
    let author = node_a.default_author(ids::ALICE)?;
    node_a.create_namespace(ids::ALICE, ids::ALICE).await?;
    let name = EntryPath::new("contact/name")?;
    node_a
        .write(ids::ALICE, ids::ALICE, author, &name, b"Alice")
        .await?;
    let directory_ticket = directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let sibling_dir = join_identity(&node_b, ids::ALICE, directory_ticket).await?;
    directory.add_device(node_b.node_id()).await?;
    sibling_dir.add_device(node_b.node_id()).await?;
    let ticket = node_a
        .share_ticket(
            ids::ALICE,
            ids::ALICE,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    node_b
        .import_namespace(ids::ALICE, ids::ALICE, ticket)
        .await?;
    assert!(
        wait_entry_is(&node_b, ids::ALICE, ids::ALICE, &name, b"Alice").await?,
        "node stopped syncing after an extra handler panicked"
    );

    node_a.shutdown().await?;
    node_b.shutdown().await?;
    Ok(())
}

/// A dial under an ALPN the node did not register fails, and the registered
/// handler never runs.
#[tokio::test(flavor = "multi_thread")]
async fn unregistered_alpn_is_refused() -> Result<()> {
    let echo = EchoHandler::default();
    let node_a = SyncNode::spawn_with(
        vec![(ECHO_ALPN.to_vec(), Box::new(echo.clone()))],
        SpawnOptions::memory(),
    )
    .await?;
    let node_b = memory_node().await?;

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

/// An extra protocol claiming a built-in ALPN, or the same ALPN as another
/// extra, fails the spawn with the typed collision error; no node starts.
#[tokio::test(flavor = "multi_thread")]
async fn alpn_collisions_are_refused_at_spawn() -> Result<()> {
    for reserved in BUILT_IN_ALPNS {
        let err = SyncNode::spawn_with(
            vec![(reserved.to_vec(), Box::new(EchoHandler::default()))],
            SpawnOptions::memory(),
        )
        .await
        .expect_err("a built-in ALPN must be refused at spawn");
        let taken: &AlpnTaken = err.downcast_ref().expect("typed AlpnTaken error");
        assert_eq!(taken.alpn, reserved);
    }

    let err = SyncNode::spawn_with(
        vec![
            (ECHO_ALPN.to_vec(), Box::new(EchoHandler::default())),
            (ECHO_ALPN.to_vec(), Box::new(EchoHandler::default())),
        ],
        SpawnOptions::memory(),
    )
    .await
    .expect_err("a duplicate extra ALPN must be refused at spawn");
    let taken: &AlpnTaken = err.downcast_ref().expect("typed AlpnTaken error");
    assert_eq!(taken.alpn, ECHO_ALPN);
    Ok(())
}
