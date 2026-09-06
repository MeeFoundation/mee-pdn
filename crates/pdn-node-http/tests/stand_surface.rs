//! The surface's own bounds against a node of the stand: the debug gate,
//! the routes it gates, and the body ceiling. Absent is the point of the
//! gate — a route answering "unauthorized" instead of 404 would be a
//! surface present in a deployment that must not have one. Ignored by
//! default: `just test-docker` builds the image and runs the suite.

use anyhow::Result;
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::MAX_REQUEST_BODY_BYTES;

mod common;
use common::{Method, Stand};

/// One of every debug route, by method and path — a hosted identity is not
/// needed, because the gate decides before any handler runs.
const DEBUG_ROUTES: &[(Method, &str)] = &[
    (Method::Get, "/debug/status"),
    (Method::Post, "/debug/identities"),
    (Method::Get, "/debug/identities"),
    (Method::Post, "/debug/identities/aa/linking-invite"),
    (Method::Post, "/debug/link"),
    (Method::Post, "/debug/identities/aa/invite"),
    (Method::Post, "/debug/identities/aa/establish"),
    (Method::Get, "/debug/identities/aa/connections"),
    (Method::Post, "/debug/identities/aa/grants/bb"),
    (Method::Get, "/debug/identities/aa/grants/bb"),
    (Method::Get, "/debug/identities/aa/own-grants/bb"),
    (Method::Delete, "/debug/identities/aa/grants/bb/cc"),
    (Method::Get, "/debug/data/aa"),
    (Method::Put, "/debug/data/aa/contact/email"),
    (Method::Get, "/debug/data/aa/contact/email"),
];

/// Without the flag the node answers liveness and readiness, and every
/// `/debug/` route is absent (404) — the image's own default.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn live_is_200_and_debug_is_absent_without_the_flag() -> Result<()> {
    let stand = Stand::new();
    let node = stand.spawn_without_debug("gated").await?;

    assert_eq!(node.get("/live").await?.status, StatusCode::OK);
    assert_eq!(node.get("/ready").await?.status, StatusCode::OK);

    // Paired deny: without the flag not one `/debug/` route exists.
    for (method, path) in DEBUG_ROUTES {
        let answer = node.request(*method, path, Bytes::new()).await?;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method:?} {path} must be absent without the flag"
        );
    }
    Ok(())
}

/// With the flag on, every route in the list answers something other than
/// 404 — so the list is a list of real routes, not of typos that would pass
/// the gate assertion for the wrong reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn every_gated_route_exists_with_the_flag() -> Result<()> {
    let stand = Stand::new();
    let node = stand.spawn("gated").await?;

    for (method, path) in DEBUG_ROUTES {
        let answer = node.request(*method, path, Bytes::new()).await?;
        assert_ne!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method:?} {path} must exist with the flag: {}",
            answer.text()
        );
    }
    Ok(())
}

/// A body past the ceiling is refused by the router, before the data service
/// is reached — so the limit is the host's own and not the engine's.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn an_oversized_body_is_refused_before_its_handler() -> Result<()> {
    let stand = Stand::new();
    let node = stand.spawn("bounded").await?;
    let alice = node.create_identity().await?;

    let oversized = vec![b'x'; MAX_REQUEST_BODY_BYTES + 1];
    let answer = node
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            Bytes::from(oversized),
        )
        .await?;
    assert_eq!(
        answer.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "an oversized body must be refused, got {}: {}",
        answer.status,
        answer.text()
    );

    // And the entry it addressed stays absent: the refusal happened before
    // any handler, not after a partial write.
    let after = node
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(after.status, StatusCode::NOT_FOUND);
    Ok(())
}
