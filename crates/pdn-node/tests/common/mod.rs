//! Helpers shared by this crate's scenario tests.
//!
//! Runtime-level test support lives here rather than in `test-utils`: that
//! crate serves the layer below (`data-layer`'s own tests depend on it), so a
//! helper needing `Runtime` would force it to depend on `pdn-node` and make
//! the lower layer's tests compile the runtime above them.
//!
//! The linking helpers drive the dialogue raw — its ALPN, framing, and
//! message shapes mirrored here on purpose: they are the wire contract of
//! ADR-0012, and a silent drift in the protocol must break these tests.
// Each test binary includes this module and uses its own subset of the
// helpers; what one binary leaves unused is not dead code of the crate.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use data_layer::{Connection, DocTicket, PrivateMetadataStore, RecvStream, SendStream, SyncNode};
use pdn_node::{
    ConnectionsService as _, IdentityService as _, InvitePayload, LinkingPayload, PeerGrant,
    Runtime,
};
use pdn_types::PdnId;
use test_utils::{eventually, TIMEOUT};

/// The linking ALPN, pinned by the tests on purpose (ADR-0012).
pub const LINKING_ALPN: &[u8] = b"/pdn/linking/0";

/// Ceiling on one linking wire frame — mirrors the protocol's own bound.
const MAX_FRAME_LEN: u32 = 64 * 1024;

/// Write one length-prefixed frame of the ceremonies' wire framing.
pub async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())?;
    ensure!(len <= MAX_FRAME_LEN, "frame too large: {len} bytes");
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Read one length-prefixed frame of the ceremonies' wire framing.
pub async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    ensure!(len <= MAX_FRAME_LEN, "frame too large: {len} bytes");
    let mut bytes = vec![0u8; usize::try_from(len)?];
    recv.read_exact(&mut bytes).await?;
    Ok(bytes)
}

/// Run the linking dialogue raw from a bare node: dial the payload's
/// address on the linking ALPN, present the secret, and return the reply's
/// directory and data write tickets — what `link` does, without a runtime
/// around it. The request mirrors the protocol's `{version, secret}`
/// message (postcard encodes the struct exactly as this tuple), the reply
/// its `{directory, data}`.
pub async fn dial_linking(
    node: &SyncNode,
    payload: &LinkingPayload,
) -> Result<(DocTicket, DocTicket)> {
    let connection = node
        .dial_handle()
        .connect(payload.inviter_addr.clone(), LINKING_ALPN)
        .await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    write_frame(
        &mut send,
        &postcard::to_stdvec(&(payload.version, payload.secret))?,
    )
    .await?;
    send.finish()?;
    let reply = read_frame(&mut recv)
        .await
        .context("linking refused by the inviter")?;
    let (directory, data): (DocTicket, DocTicket) = postcard::from_bytes(&reply)?;
    connection.close(0u32.into(), b"done");
    Ok((directory, data))
}

/// Present `payload`'s secret and never read the reply — the lost-response
/// dialer of the linking convergence scenario. The connection is handed
/// back still open (its receive half already dropped), so the caller
/// decides when to drop it; the inviter's reply is lost either way.
pub async fn dial_linking_without_reading(
    node: &SyncNode,
    payload: &LinkingPayload,
) -> Result<Connection> {
    let connection = node
        .dial_handle()
        .connect(payload.inviter_addr.clone(), LINKING_ALPN)
        .await?;
    let (mut send, _dropped_recv) = connection.open_bi().await?;
    write_frame(
        &mut send,
        &postcard::to_stdvec(&(payload.version, payload.secret))?,
    )
    .await?;
    send.finish()?;
    Ok(connection)
}

/// A store-level probe of `identity`'s directory on `runtime`: a bare node
/// that links raw — the same act a linking device performs — and imports
/// the directory from the reply, so everything it reads afterwards is what
/// any device of the identity reads. Patient like [`establish_patiently`],
/// minting a fresh invite per retry. Note the inviter registers the probe
/// as a device, so device-set assertions use contains/exact-with-probe,
/// never counts that forget it.
pub async fn link_probe(
    runtime: &Runtime,
    identity: PdnId,
) -> Result<(SyncNode, PrivateMetadataStore)> {
    let node = SyncNode::spawn().await?;
    let deadline = Instant::now() + TIMEOUT;
    let (directory_ticket, _data_ticket) = loop {
        let payload = runtime.identity().linking_invite(identity, None).await?;
        match dial_linking(&node, &payload).await {
            Ok(tickets) => break tickets,
            Err(err) if Instant::now() > deadline => return Err(err),
            Err(_cold_or_burned) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    let directory = PrivateMetadataStore::import(&node, directory_ticket).await?;
    Ok((node, directory))
}

/// Link `linker` into `identity` with patience for a cold transport: mint a
/// fresh invite on `inviter` per attempt (the inviter burns the secret at
/// presentation, so a failure at or after the burn is recoverable only by a
/// fresh invite) and retry until [`TIMEOUT`]. Assertions about one specific
/// invite — that *this* secret is refused, or must be the one burned — call
/// `link` directly instead.
pub async fn link_patiently(linker: &Runtime, inviter: &Runtime, identity: PdnId) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let payload = inviter.identity().linking_invite(identity, None).await?;
        match linker.identity().link(payload, TIMEOUT).await {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() > deadline => return Err(err),
            Err(_cold_or_burned) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Establish with patience for a cold transport: present `invite`, and on
/// failure keep retrying with a **freshly minted** invite until [`TIMEOUT`].
///
/// A fresh process's first dials can be slowed or dropped — the cold-start
/// costs described on [`test_utils::TIMEOUT`] — so success-path
/// establishments retry. The retry mints a new invite rather than replaying
/// the old secret: the inviter burns the secret before replying (ADR-0011),
/// so a failure at or after the burn is recoverable only by a fresh invite —
/// re-presenting the same one would be refused forever. This mirrors the
/// QR-rotation a host performs (the scanner captures the currently
/// displayed, freshly minted invite). Assertions about one specific invite —
/// that *this* secret is refused, or must be the one burned — call
/// `establish` directly instead.
pub async fn establish_patiently(
    scanner: &Runtime,
    scanner_id: PdnId,
    inviter: &Runtime,
    inviter_id: PdnId,
    mut invite: InvitePayload,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match scanner.connections().establish(scanner_id, invite).await {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() > deadline => return Err(err),
            Err(_cold_or_burned) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                invite = inviter.connections().invite(inviter_id, None).await?;
            }
        }
    }
}

/// Publish a grant of `issuer`'s namespace from the giving side's hosted
/// identity toward the receiving side's, and hand back the grant as the
/// receiver reads it once it has crossed the connection's metadata pair.
///
/// The crossing is a poll, not a wait: a grant's ticket payload is a blob and
/// lags the record that names it, so `read_grants` omits it until it lands.
pub async fn granted_patiently(
    gives: &Runtime,
    gives_id: PdnId,
    receives: &Runtime,
    receives_id: PdnId,
    issuer: PdnId,
) -> Result<PeerGrant> {
    gives
        .connections()
        .publish_grant(gives_id, receives_id, issuer)
        .await?;
    let crossed = eventually(|| async {
        Ok(receives
            .connections()
            .read_grants(receives_id, gives_id)
            .await?
            .iter()
            .any(|grant| grant.issuer == issuer))
    })
    .await?;
    ensure!(crossed, "the grant did not reach the peer over the pair");
    receives
        .connections()
        .read_grants(receives_id, gives_id)
        .await?
        .into_iter()
        .find(|grant| grant.issuer == issuer)
        .context("grant just observed, then gone")
}
