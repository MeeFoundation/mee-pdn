//! The whole stand scenario over HTTP alone: two hosts, each with its own
//! embedded runtime, driven only through their debug surfaces — identities
//! created, a connection established, a scoped grant published, an entry
//! written and read back through the grantee, and the grant withdrawn again
//! so the access it opened closes.
//!
//! No step reaches into a runtime directly, and no namespace ticket appears
//! anywhere in the test: the grantee's read works because the runtime binds
//! what the grant names, which is the property the container stand exists to
//! demonstrate. Waiting for convergence is repeating the read — the only
//! means the surface offers.
//!
//! The paired denials of `code-practices/access-control-tests.md` sit beside
//! the authorized read: the outsider — a third host with no connection and
//! no grant — is refused as unknown, and the claims the grant withholds are
//! absent from the grantee's view after a proven second replication wave.
//! The practice's other tightest party, a holder of the store's ticket
//! without a read capability, cannot be constructed here at all: no ticket
//! crosses this surface, which the grant read asserts.
//!
//! What travels between the hosts is iroh, not HTTP: each request acts on
//! the runtime of the host serving it, and the invite payload moves between
//! them through this test — the caller — as a code moves between two screens
//! through a person.

use std::cell::RefCell;

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::shapes::{Connections, Entries, PeerGrants};

mod common;
use common::{body, entry_answers, entry_reads, eventually, grant_on, Host};

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, with its denials in the same place
async fn the_whole_scenario_runs_over_http() -> Result<()> {
    let inviter = Host::spawn().await?;
    let scanner = Host::spawn().await?;
    let outsider = Host::spawn().await?;

    let x = inviter.create_identity().await?;
    let y = scanner.create_identity().await?;

    // Establishment. The payload crosses as an opaque token: the bytes one
    // host answered with, handed to the other unread, so this test never
    // comes to depend on which fields a payload has. The lifetime is named
    // explicitly; omitting it leaves the runtime's own short default.
    let payload = inviter
        .post(
            &format!("/debug/identities/{x}/invite?lifetime_secs=60"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    scanner
        .post(&format!("/debug/identities/{y}/establish"), payload)
        .await?
        .ok()?;

    // Both sides record the connection, each read from its own host.
    let inviter_side: Connections = inviter
        .get(&format!("/debug/identities/{x}/connections"))
        .await?
        .json()?;
    assert!(
        inviter_side.connections.contains(&y),
        "the inviter must record the connection: {inviter_side:?}"
    );
    let scanner_side: Connections = scanner
        .get(&format!("/debug/identities/{y}/connections"))
        .await?
        .json()?;
    assert!(
        scanner_side.connections.contains(&x),
        "the scanner must record the connection: {scanner_side:?}"
    );

    // X's data: the claim the grant will name, and one it will withhold.
    inviter
        .put(
            &format!("/debug/data/{x}/contact/email"),
            body(b"x@example.org"),
        )
        .await?
        .ok()?;
    inviter
        .put(&format!("/debug/data/{x}/notes/diary"), body(b"dear diary"))
        .await?
        .ok()?;

    // The grant: read-only on exactly `contact/email`.
    inviter
        .publish_grant(x, y, &grant_on(x, "contact/email", false)?)
        .await?
        .ok()?;

    // The grantee reads the capability over the surface. The answer is
    // accumulated inside the poll: a record whose ticket payload is still
    // arriving reads as no grant at all — the very transient the poll exists
    // for — so a second read afterwards would not be the same read.
    let observed = RefCell::new(None::<Bytes>);
    let arrived = eventually(|| async {
        let raw = scanner
            .get(&format!("/debug/identities/{y}/grants/{x}"))
            .await?
            .ok()?;
        let grants: PeerGrants = serde_json::from_slice(&raw)?;
        let found = grants.grants.iter().any(|grant| grant.issuer == x);
        if found {
            *observed.borrow_mut() = Some(raw);
        }
        Ok(found)
    })
    .await?;
    assert!(arrived, "the grant did not reach the grantee over the pair");
    let raw = observed
        .into_inner()
        .context("the poll reported the grant and handed back nothing")?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        !text.contains("ticket"),
        "no namespace ticket may cross the surface: {text}"
    );
    let grants: PeerGrants = serde_json::from_slice(&raw)?;
    let capability = grants
        .grants
        .iter()
        .find(|grant| grant.issuer == x)
        .context("the poll's answer must carry the grant it reported")?;
    assert_eq!(capability.audience, y);
    assert!(
        capability.claims.iter().all(|claim| !claim.write),
        "the published grant is read-only: {capability:?}"
    );

    // Allowed: the granted entry reads back through the grantee, waited for
    // by repeating the read.
    entry_reads(&scanner, x, "contact/email", b"x@example.org")
        .await
        .context("the granted entry did not reach the grantee")?;

    // Denied (outsider): a host that never connected to X and holds no
    // grant is refused as unknown — a refusal, not an absence, so the
    // assertion cannot pass by way of a renamed route.
    let refused = outsider
        .get(&format!("/debug/data/{x}/contact/email"))
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
            &format!("/debug/data/{x}/contact/email"),
            body(b"x@new.example.org"),
        )
        .await?
        .ok()?;
    entry_reads(&scanner, x, "contact/email", b"x@new.example.org")
        .await
        .context("the sentinel update did not reach the grantee")?;

    // Denied (existence hidden): after that wave, the grantee's view of X's
    // namespace carries exactly the granted claim.
    let listed: Entries = scanner.get(&format!("/debug/data/{x}")).await?.json()?;
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
    let withheld = scanner.get(&format!("/debug/data/{x}/notes/diary")).await?;
    assert_eq!(
        withheld.status,
        StatusCode::NOT_FOUND,
        "a withheld claim must read as absent, got {}: {}",
        withheld.status,
        withheld.text()
    );

    // The listing narrows to whole path components, on the issuer's own side
    // where both claims are present — the filter, not the grant, is what is
    // read here.
    let narrowed: Entries = inviter
        .get(&format!("/debug/data/{x}?prefix=notes"))
        .await?
        .json()?;
    let narrowed_paths: Vec<String> = narrowed
        .entries
        .iter()
        .map(|entry| entry.path.to_string())
        .collect();
    assert_eq!(
        narrowed_paths,
        vec!["notes/diary".to_owned()],
        "a prefix listing must carry exactly what it names"
    );
    // A mistyped parameter is refused rather than silently widening the
    // listing back to the whole namespace.
    let mistyped = inviter.get(&format!("/debug/data/{x}?prefx=notes")).await?;
    assert_eq!(
        mistyped.status,
        StatusCode::BAD_REQUEST,
        "an unknown query parameter must be refused, got {}: {}",
        mistyped.status,
        mistyped.text()
    );

    // Withdrawal, the counterpart of the grant above: the grantee's binder
    // forgets what the grant brought in, so the issuer resolves to nothing
    // there again — a refusal, not an empty answer. The issuer keeps its own
    // data throughout, which is what says withdrawal narrowed access and did
    // not delete anything.
    inviter
        .delete(&format!("/debug/identities/{x}/grants/{y}/{x}"))
        .await?
        .ok()?;
    entry_answers(&scanner, x, "contact/email", StatusCode::CONFLICT)
        .await
        .context("the withdrawn namespace stayed bound on the grantee")?;
    let after: PeerGrants = scanner
        .get(&format!("/debug/identities/{y}/grants/{x}"))
        .await?
        .json()?;
    assert!(
        after.grants.iter().all(|grant| grant.issuer != x),
        "the withdrawn grant must be gone from the grantee's view: {after:?}"
    );
    let issuer_side = inviter
        .get(&format!("/debug/data/{x}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        issuer_side,
        Bytes::from_static(b"x@new.example.org"),
        "withdrawal must leave the issuer's own entry untouched"
    );

    outsider.shutdown().await?;
    scanner.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}
