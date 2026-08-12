//! The stand's scenarios: identities meet, grant, replicate and withdraw
//! across containers, a device joins an identity, and a device goes away.
//!
//! Every node is its own process in its own container, reached only over its
//! published HTTP port. No step reaches into a runtime, and no namespace
//! ticket appears anywhere: a grantee reads because the runtime binds what
//! the grant names, which is the property the stand exists to demonstrate.
//! Waiting for convergence is repeating the read — the only means the
//! surface offers.
//!
//! What travels between the nodes is iroh, not HTTP: each request acts on
//! the runtime of the node serving it, and a ceremony payload moves between
//! them through the test — the caller — as a code moves between two screens
//! through a person.
//!
//! Ignored by default: the suite needs a container daemon and a built image,
//! and `just test-docker` builds the image and runs it.

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::shapes::{Connections, Entries, GrantCapability, HostedIdentities, PeerGrants};

mod common;
use common::{body, entry_answers, entry_reads, eventually, grant_on, Stand, CONVERGENCE_BUDGET};

/// The whole stand scenario with its paired denials: two identities meet,
/// one grants a subset of its data, the grantee reads exactly that subset,
/// an outsider is refused, and the withdrawal closes the access the grant
/// opened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one scenario, with its denials in the same place
async fn the_whole_scenario_runs_across_containers() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let scanner = stand.spawn("scanner").await?;
    let outsider = stand.spawn("outsider").await?;

    let alice = inviter.create_identity().await?;
    let bob = scanner.create_identity().await?;

    // Establishment. The payload crosses as an opaque token: the bytes one
    // node answered with, handed to the other unread, so this test never
    // comes to depend on which fields a payload has. The lifetime is named
    // explicitly; omitting it leaves the runtime's own short default.
    let payload = inviter
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    scanner
        .post(&format!("/debug/identities/{bob}/establish"), payload)
        .await?
        .ok()?;

    // Both sides record the connection, each read from its own node.
    let inviter_side: Connections = inviter
        .get(&format!("/debug/identities/{alice}/connections"))
        .await?
        .json()?;
    assert!(
        inviter_side.connections.contains(&bob),
        "the inviter must record the connection: {inviter_side:?}"
    );
    let scanner_side: Connections = scanner
        .get(&format!("/debug/identities/{bob}/connections"))
        .await?
        .json()?;
    assert!(
        scanner_side.connections.contains(&alice),
        "the scanner must record the connection: {scanner_side:?}"
    );

    // Alice's data: the claim the grant will name, and one it will withhold.
    inviter
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@example.org"),
        )
        .await?
        .ok()?;
    inviter
        .put(
            &format!("/debug/data/{alice}/notes/diary"),
            body(b"dear diary"),
        )
        .await?
        .ok()?;

    // The grant: read-only on exactly `contact/email`.
    inviter
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false)?)
        .await?
        .ok()?;

    // The grantee reads the capability over the surface. The answer is
    // accumulated inside the poll: a record whose ticket payload is still
    // arriving reads as no grant at all — the very transient the poll exists
    // for — so a second read afterwards would not be the same read.
    let arrived = eventually(CONVERGENCE_BUDGET, || async {
        let raw = scanner
            .get(&format!("/debug/identities/{bob}/grants/{alice}"))
            .await?
            .ok()?;
        let grants: PeerGrants = serde_json::from_slice(&raw)?;
        Ok(grants.grants.iter().any(|grant| grant.issuer == alice))
    })
    .await?;
    assert!(arrived, "the grant did not reach the grantee over the pair");
    let raw = scanner
        .get(&format!("/debug/identities/{bob}/grants/{alice}"))
        .await?
        .ok()?;
    // No namespace ticket may cross the surface, whatever a leaked field
    // happened to be named: `GrantCapability` denies unknown fields, so a
    // response carrying anything beyond issuer/audience/claims — ticket
    // included — fails to decode here rather than passing unnoticed.
    let grants: PeerGrants = serde_json::from_slice(&raw).context(
        "the grants response carried an unexpected field — a namespace ticket, most likely",
    )?;
    let capability = grants
        .grants
        .iter()
        .find(|grant| grant.issuer == alice)
        .context("the poll's answer must carry the grant it reported")?;
    let GrantCapability {
        issuer: _,
        audience,
        claims,
    } = capability;
    assert_eq!(*audience, bob);
    assert!(
        claims.iter().all(|claim| !claim.write),
        "the published grant is read-only: {capability:?}"
    );

    // Allowed: the granted entry reads back through the grantee, waited for
    // by repeating the read.
    entry_reads(&scanner, alice, "contact/email", b"alice@example.org")
        .await
        .context("the granted entry did not reach the grantee")?;

    // Denied (outsider): a node that never connected to Alice and holds no grant
    // is refused as unknown — a refusal, not an absence, so the assertion
    // cannot pass by way of a renamed route.
    let refused = outsider
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an outsider must be refused as unknown, got {}: {}",
        refused.status,
        refused.text()
    );

    // Sentinel: an update to the granted claim proves a second replication
    // wave end to end, which is what orders the absence assertion below.
    inviter
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@new.example.org"),
        )
        .await?
        .ok()?;
    entry_reads(&scanner, alice, "contact/email", b"alice@new.example.org")
        .await
        .context("the sentinel update did not reach the grantee")?;

    // Denied (existence hidden): after that wave, the grantee's view of
    // Alice's namespace carries exactly the granted claim.
    let listed: Entries = scanner.get(&format!("/debug/data/{alice}")).await?.json()?;
    let paths: Vec<String> = listed
        .entries
        .iter()
        .map(|entry| entry.path.to_string())
        .collect();
    assert_eq!(
        paths,
        vec!["contact/email".to_owned()],
        "the grantee's view must carry exactly the granted subset"
    );
    let withheld = scanner
        .get(&format!("/debug/data/{alice}/notes/diary"))
        .await?;
    assert_eq!(
        withheld.status,
        StatusCode::NOT_FOUND,
        "a withheld claim must read as absent, got {}: {}",
        withheld.status,
        withheld.text()
    );

    // Withdrawal, the counterpart of the grant above: the grantee's binder
    // forgets what the grant brought in, so the issuer resolves to nothing
    // there again — a refusal, not an empty answer. The issuer keeps its own
    // data throughout, which is what says withdrawal narrowed access and did
    // not delete anything.
    inviter
        .delete(&format!("/debug/identities/{alice}/grants/{bob}/{alice}"))
        .await?
        .ok()?;
    entry_answers(&scanner, alice, "contact/email", StatusCode::CONFLICT)
        .await
        .context("the withdrawn namespace stayed bound on the grantee")?;
    let after: PeerGrants = scanner
        .get(&format!("/debug/identities/{bob}/grants/{alice}"))
        .await?
        .json()?;
    assert!(
        after.grants.iter().all(|grant| grant.issuer != alice),
        "the withdrawn grant must be gone from the grantee's view: {after:?}"
    );
    let issuer_side = inviter
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        issuer_side,
        Bytes::from_static(b"alice@new.example.org"),
        "withdrawal must leave the issuer's own entry untouched"
    );
    Ok(())
}

/// A device joins an identity: the linking payload is minted on the node of
/// the identity's first device and consumed on a second, which then reports
/// the identity among the ones it hosts and reads what was written before it
/// joined.
///
/// The paired denials sit beside the successful link: the same payload
/// presented a second time is refused — its secret is burnt — and the node
/// that presented it hosts nothing afterwards, and a node that never linked
/// is refused as unknown when it addresses the identity's namespace. The
/// stranger is a node of its own rather than the bystander, whose refused
/// attempt could leave a residue that passes the last check for the wrong
/// reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_device_joins_across_containers() -> Result<()> {
    let stand = Stand::new();
    let first = stand.spawn("first").await?;
    let second = stand.spawn("second").await?;
    let bystander = stand.spawn("bystander").await?;
    let stranger = stand.spawn("stranger").await?;

    let alice = first.create_identity().await?;
    first
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"written before the link"),
        )
        .await?
        .ok()?;

    let payload = first
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    // The budget of the whole act — dialogue plus catch-up — named
    // explicitly; omitting it leaves the surface's own default.
    second
        .post("/debug/link?timeout_secs=60", payload.clone())
        .await?
        .ok()?;

    // The second node hosts the identity now.
    let hosted: HostedIdentities = second.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&alice),
        "the linked node must report the alice: {hosted:?}"
    );

    // And reads what the first device wrote before it joined.
    entry_reads(&second, alice, "contact/email", b"written before the link")
        .await
        .context("the linked device did not catch up on the entry written before the link")?;

    // Denied (a replayed payload): the secret was burnt by the link above,
    // so a second presentation is refused — and the refusal is a refusal,
    // distinguishable from a node that never reached the inviter.
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

    // Denied (a node that never linked): addressing the identity's namespace
    // is refused as unknown, not answered as absent.
    let outsider = stranger
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        outsider.status,
        StatusCode::CONFLICT,
        "a node that never linked must be refused as unknown, got {}: {}",
        outsider.status,
        outsider.text()
    );
    Ok(())
}

/// A granted peer keeps converging after the device that published the grant
/// is stopped: a sibling device of the same identity serves the namespace.
///
/// The property is that the issuer's whole device set is reachable, not only
/// the publishing one, and this is the only place it is proven across
/// processes: a contact derived from a device record carries an endpoint id
/// alone, and whether that resolves is a question about a real network. A
/// failure to converge after the stop is not answered by a longer budget —
/// it says device records must carry addresses, which is a change of its own.
///
/// The denial beside it: the failover must not widen access. A node that
/// never connected to the issuer is still refused afterwards, so "the peer
/// reads" cannot be satisfied by a node that serves whoever asks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one failover, with its denial in the same place
async fn a_stopped_device_does_not_stop_the_connection() -> Result<()> {
    let stand = Stand::new();
    let publisher = stand.spawn("alice-publisher").await?;
    let sibling = stand.spawn("alice-sibling").await?;
    let audience = stand.spawn("audience").await?;
    let outsider = stand.spawn("outsider").await?;

    // The issuer on two devices: the second joins through the linking
    // ceremony, the way a device joins.
    let alice = publisher.create_identity().await?;
    let payload = publisher
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    sibling
        .post("/debug/link?timeout_secs=60", payload)
        .await?
        .ok()?;

    // The connection, established from the publishing device.
    let bob = audience.create_identity().await?;
    let invite = publisher
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    audience
        .post(&format!("/debug/identities/{bob}/establish"), invite)
        .await?
        .ok()?;

    // The grant, published from that same device and read by the audience,
    // so the failover starts from a connection that demonstrably works.
    publisher
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"published from the first device"),
        )
        .await?
        .ok()?;
    publisher
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false)?)
        .await?
        .ok()?;
    entry_reads(
        &audience,
        alice,
        "contact/email",
        b"published from the first device",
    )
    .await
    .context("the audience never read the granted entry before the stop")?;

    // The device that published the grant goes away — and is gone: a device
    // still running would leave the convergence below provable by the very
    // one this scenario removes, which is the whole assertion. The daemon is
    // asked, not the network: the stopped container's published port is
    // released, and a probe to the address it used can be answered by a live
    // node that was given the same port afterwards.
    publisher.stop().await?;
    assert!(
        !publisher.is_running().await?,
        "the device this scenario stops is still running"
    );

    // The sibling writes, and the audience converges on it — reaching a
    // device of the issuer whose address it was never given.
    sibling
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"served by the sibling"),
        )
        .await?
        .ok()?;
    if let Err(err) = entry_reads(&audience, alice, "contact/email", b"served by the sibling").await
    {
        let logs = format!(
            "{}\n{}",
            audience.diagnostics().await,
            sibling.diagnostics().await
        );
        return Err(err.context(logs));
    }

    // Denied: the failover widened nothing. A node with no connection and no
    // grant is still refused as unknown, not answered as absent.
    let refused = outsider
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an outsider must stay refused after the failover, got {}: {}",
        refused.status,
        refused.text()
    );
    Ok(())
}
