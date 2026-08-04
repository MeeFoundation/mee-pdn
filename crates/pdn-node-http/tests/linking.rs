//! A device joins an identity over HTTP: the linking payload is minted on
//! the host of the identity's first device and consumed on a second host,
//! which then reports the identity among the ones it hosts and reads what
//! was written before it joined.
//!
//! The payload crosses as an opaque token through this test, the way a
//! person carries a code from one screen to another; the dialogue itself and
//! the catch-up that follows it run over the runtimes' iroh connections.
//!
//! The paired denials sit beside the successful link: the same payload
//! presented a second time is refused — its secret is burnt — and the host
//! that presented it hosts nothing afterwards, and a host that never linked
//! is refused as unknown when it addresses the identity's namespace.

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::shapes::HostedIdentities;

mod common;
use common::{body, entry_reads, Host};

#[tokio::test(flavor = "multi_thread")]
async fn a_device_joins_over_http() -> Result<()> {
    let first = Host::spawn().await?;
    let second = Host::spawn().await?;
    let bystander = Host::spawn().await?;

    let identity = first.create_identity().await?;
    first
        .put(
            &format!("/debug/data/{identity}/contact/email"),
            body(b"written before the link"),
        )
        .await?
        .ok()?;

    let payload = first
        .post(
            &format!("/debug/identities/{identity}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    // The budget of the whole act — dialogue plus catch-up — named
    // explicitly; omitting it leaves the surface's own default.
    second
        .post("/debug/link?timeout_secs=30", payload.clone())
        .await?
        .ok()?;

    // The second host hosts the identity now.
    let hosted: HostedIdentities = second.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&identity),
        "the linked host must report the identity: {hosted:?}"
    );

    // And reads what the first device wrote before it joined.
    entry_reads(
        &second,
        identity,
        "contact/email",
        b"written before the link",
    )
    .await
    .context("the linked device did not catch up on the entry written before the link")?;

    // Denied (a replayed payload): the secret was burnt by the link above,
    // so a second presentation is refused — and the refusal is a refusal,
    // distinguishable from a host that never reached the inviter.
    let refused = bystander.post("/debug/link", payload).await?;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a replayed linking payload must be refused, got {}: {}",
        refused.status,
        refused.text()
    );
    let nothing: HostedIdentities = bystander.get("/debug/identities").await?.json()?;
    assert!(
        nothing.identities.is_empty(),
        "a refused link must leave nothing behind: {nothing:?}"
    );

    // Denied (a host that never linked): addressing the identity's namespace
    // is refused as unknown, not answered as absent.
    let outsider = bystander
        .get(&format!("/debug/data/{identity}/contact/email"))
        .await?;
    assert_eq!(
        outsider.status,
        StatusCode::CONFLICT,
        "a host that never linked must be refused as unknown, got {}: {}",
        outsider.status,
        outsider.text()
    );

    bystander.shutdown().await?;
    second.shutdown().await?;
    first.shutdown().await?;
    Ok(())
}
