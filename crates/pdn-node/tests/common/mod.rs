//! Helpers shared by this crate's scenario tests (runtime-level, so not in
//! `test-utils`, which `data-layer`'s tests depend on). The linking helpers
//! drive the dialogue raw — ALPN, framing, and message shapes mirrored on
//! purpose: they are the wire contract of ADR-0012, and a silent drift must
//! break these tests.
// Each test binary uses its own subset of the helpers.
#![allow(dead_code)]

use anyhow::{ensure, Context, Result};
use data_layer::{
    AcceptError, Connection, DialHandle, DocTicket, PrivateMetadataStore, ProtocolHandler,
    RecvStream, SendStream, SyncNode,
};
use pdn_node::{IdentityService as _, LinkingPayload, Runtime, SpawnOptions};
use pdn_types::PdnId;
use test_utils::{memory_node, TIMEOUT};

/// A runtime on memory storage.
pub async fn memory_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions::memory()).await
}

/// The linking ALPN, pinned by the tests on purpose (ADR-0012).
pub const LINKING_ALPN: &[u8] = b"/pdn/linking/0";

/// The pod join ALPN, pinned by the tests on purpose.
pub const POD_JOIN_ALPN: &[u8] = b"/pdn/pod-join/0";

/// An inviter of either ceremony that reads the request and never
/// answers; without a ceiling over the dialogue a dialer waits for the
/// transport's idle timeout.
#[derive(Debug)]
pub struct HungInviter;

impl ProtocolHandler for HungInviter {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Ok((_send, mut recv)) = connection.accept_bi().await {
            let _request = read_frame(&mut recv).await;
            // Never answer; hold until the dialer closes or gives up.
            connection.closed().await;
        }
        Ok(())
    }
}

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

/// Run the linking dialogue raw from a bare node. The request mirrors the
/// protocol's `{version, secret}` message (postcard encodes the struct
/// exactly as this tuple), the reply its `{pms}`.
pub async fn dial_linking(node: &SyncNode, payload: &LinkingPayload) -> Result<DocTicket> {
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
    let pms: DocTicket = postcard::from_bytes(&reply)?;
    connection.close(0u32.into(), b"done");
    Ok(pms)
}

/// Present `payload`'s secret and never read the reply. The connection is
/// handed back still open, its receive half already dropped.
pub async fn dial_linking_without_reading(
    node: &SyncNode,
    payload: &LinkingPayload,
) -> Result<Connection> {
    dial_linking_without_reading_from(&node.dial_handle(), payload).await
}

pub async fn dial_linking_without_reading_from(
    dial: &DialHandle,
    payload: &LinkingPayload,
) -> Result<Connection> {
    let connection = dial
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

/// A store-level probe of `identity`'s PMS: a bare node that links
/// raw and confirms itself, so everything it reads is what any device of
/// the identity reads. The probe ends up in the device set, so device-set
/// assertions never use counts that forget it.
pub async fn link_probe(
    runtime: &Runtime,
    identity: PdnId,
) -> Result<(SyncNode, PrivateMetadataStore)> {
    let node = memory_node().await?;
    node.provision_identity(identity).await?;
    let payload = runtime.identity().linking_invite(identity, None).await?;
    let pms_ticket = dial_linking(&node, &payload).await?;
    let pms = PrivateMetadataStore::import(&node, identity, pms_ticket).await?;
    // Armed as any device of the identity is: an identity's replicas are
    // judged by its own records, and a probe that armed nothing would
    // serve and pull nothing (ADR-0013).
    node.host_identity(identity, &pms)?;
    pms.confirm_device(node.node_id()).await?;
    Ok((node, pms))
}

/// Mint a fresh invite on `inviter` and link once. Assertions about one
/// specific invite call `link` directly instead.
pub async fn link_patiently(linker: &Runtime, inviter: &Runtime, identity: PdnId) -> Result<()> {
    let payload = inviter.identity().linking_invite(identity, None).await?;
    linker.identity().link(payload, TIMEOUT).await
}
