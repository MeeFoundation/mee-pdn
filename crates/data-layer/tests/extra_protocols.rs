//! The refusal and containment edges of externally supplied protocols on
//! the node assembly: a dial under an ALPN the node did not register fails
//! without running any handler, an ALPN collision (built-in or duplicate)
//! is refused at spawn, and a panicking handler is contained without
//! taking the node down. The happy path — a supplied protocol answering on
//! its ALPN next to a still-syncing built-in stack, dialed through the
//! dial handle — is exercised end to end by its real consumer, the pairing
//! protocol (pdn-node's establishment tests); these edges are what the
//! real consumer never triggers. The types come from `data-layer`'s
//! re-exported extension surface, the same way the pairing handler
//! consumes it.

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

/// A panic in an extra handler is contained: the panic is caught, that one
/// connection fails, and the node's built-in stack keeps syncing — the panic
/// does not tear the whole node down through iroh's accept loop.
#[tokio::test(flavor = "multi_thread")]
async fn panicking_extra_handler_does_not_take_down_the_node() -> Result<()> {
    let panicker = PanickingHandler::default();
    let node_a =
        SyncNode::spawn_with_protocols(vec![(PANIC_ALPN.to_vec(), Box::new(panicker.clone()))])
            .await?;
    let node_b = SyncNode::spawn().await?;

    // Dial the panicking protocol and drive a stream so its handler runs.
    // The handler panics after reading, so our side must see an error rather
    // than a clean response. The dial is patient: a process's first
    // accepted-ALPN handshake can stall on this machine (see
    // `test_utils::TIMEOUT`), and this test's dial is the binary's first.
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
    // The handler panicked before writing anything, so the dialer must come
    // away with no payload. Whether it learns that as a stream error or as a
    // clean empty end-of-stream is a teardown race, not the containment
    // property: the panic unwinds the handler's own future first, dropping
    // its `SendStream` before the connection (drop implicitly finishes the
    // stream), so a FIN is queued before the connection's close and
    // whichever reaches the dialer first decides what this read returns.
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

    // The node survived: its built-in stack still converges a replica over
    // the ordinary ticket flow.
    let author = node_a.create_author().await?;
    node_a.create_namespace(ids::ALICE).await?;
    let name = EntryPath::new("contact/name")?;
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
