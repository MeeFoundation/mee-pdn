//! Helpers shared by this crate's scenario tests (runtime-level, so not in
//! `test-utils`, which `data-layer`'s tests depend on). The linking helpers
//! drive the dialogue raw — ALPN, framing, and message shapes mirrored on
//! purpose: they are the wire contract of ADR-0012, and a silent drift must
//! break these tests.
// Each test binary uses its own subset of the helpers.
#![allow(dead_code)]

use anyhow::{ensure, Context, Result};
use data_layer::{
    Connection, DialHandle, DocTicket, PrivateMetadataStore, RecvStream, SendStream, SyncNode,
};
use pdn_node::{
    ConnectionsService as _, GrantedClaim, IdentityService as _, InvitePayload, LinkingPayload,
    Runtime, SpawnOptions,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, memory_node, TIMEOUT};

/// A runtime on memory storage.
pub async fn memory_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions::memory()).await
}

/// The linking ALPN, pinned by the tests on purpose (ADR-0012).
pub const LINKING_ALPN: &[u8] = b"/pdn/linking/0";

/// The pairing ALPN, pinned by the tests on purpose (ADR-0011).
pub const PAIRING_ALPN: &[u8] = b"/pdn/pairing/0";

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
/// exactly as this tuple), the reply its `{directory, data}`.
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

/// A store-level probe of `identity`'s directory: a bare node that links
/// raw and confirms itself, so everything it reads is what any device of
/// the identity reads. The probe ends up in the device set, so device-set
/// assertions never use counts that forget it.
pub async fn link_probe(
    runtime: &Runtime,
    identity: PdnId,
) -> Result<(SyncNode, PrivateMetadataStore)> {
    let node = memory_node().await?;
    let payload = runtime.identity().linking_invite(identity, None).await?;
    let (directory_ticket, _data_ticket) = dial_linking(&node, &payload).await?;
    let directory = PrivateMetadataStore::import(&node, directory_ticket).await?;
    directory.confirm_device(node.node_id()).await?;
    Ok((node, directory))
}

/// Mint a fresh invite on `inviter` and link once. Assertions about one
/// specific invite call `link` directly instead.
pub async fn link_patiently(linker: &Runtime, inviter: &Runtime, identity: PdnId) -> Result<()> {
    let payload = inviter.identity().linking_invite(identity, None).await?;
    linker.identity().link(payload, TIMEOUT).await
}

/// `_inviter`/`_inviter_id` are unused, kept for call-site symmetry with
/// the linking helper.
pub async fn establish_patiently(
    scanner: &Runtime,
    scanner_id: PdnId,
    _inviter: &Runtime,
    _inviter_id: PdnId,
    invite: InvitePayload,
) -> Result<()> {
    scanner.connections().establish(scanner_id, invite).await
}

/// A claim set for a scenario about the record crossing rather than about
/// what it covers.
// clippy.toml's expect relaxation reaches `#[test]` bodies only.
#[allow(clippy::expect_used)]
pub fn nominal_claims(issuer: PdnId) -> NonEmpty<GrantedClaim> {
    claims_on(
        issuer,
        &EntryPath::new("contact/email").expect("a valid path"),
        false,
    )
}

/// The claim set covering exactly `path` — read always, write when `write`.
pub fn claims_on(issuer: PdnId, path: &EntryPath, write: bool) -> NonEmpty<GrantedClaim> {
    NonEmpty::new(GrantedClaim {
        claim: pdn_node::claim_id_of(&issuer, path),
        write,
    })
}

/// Publish a grant and return once the receiver reads it whole over the
/// pair. Delivery alone: a caller that needs the grant's value reads it
/// inside its own poll, since a second read after this one is not the same
/// read.
pub async fn granted_patiently(
    gives: &Runtime,
    gives_id: PdnId,
    receives: &Runtime,
    receives_id: PdnId,
    issuer: PdnId,
    claims: NonEmpty<GrantedClaim>,
) -> Result<()> {
    gives
        .connections()
        .publish_grant(gives_id, receives_id, issuer, claims)
        .await?;
    let crossed = eventually(|| async {
        Ok(receives
            .connections()
            .read_grants(receives_id, gives_id)
            .await?
            .into_iter()
            .any(|peer_grant| peer_grant.grant.issuer == issuer))
    })
    .await?;
    ensure!(crossed, "the grant did not reach the peer over the pair");
    Ok(())
}
