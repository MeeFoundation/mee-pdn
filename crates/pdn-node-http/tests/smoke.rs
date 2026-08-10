//! Host smoke tests: liveness while the embedded runtime runs, and the
//! debug gate — off means absent.
//!
//! The gate is asserted route by route, and absent is the whole point: a
//! route that answered "unauthorized" instead of 404 would be a surface
//! present in a build that must not have one. The route names themselves
//! stay unpinned scaffolding — this test moves with them, and nothing
//! outside this repository may depend on them.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use pdn_node::Runtime;
use pdn_node_http::router;
use tower::ServiceExt as _;

/// One of every debug route, by method and path — a hosted identity is not
/// needed, because the gate decides before any handler runs.
const DEBUG_ROUTES: &[(&str, &str)] = &[
    ("GET", "/debug/status"),
    ("POST", "/debug/identities"),
    ("GET", "/debug/identities"),
    ("POST", "/debug/identities/aa/linking-invite"),
    ("POST", "/debug/link"),
    ("POST", "/debug/identities/aa/invite"),
    ("POST", "/debug/identities/aa/establish"),
    ("GET", "/debug/identities/aa/connections"),
    ("POST", "/debug/identities/aa/grants/bb"),
    ("GET", "/debug/identities/aa/grants/bb"),
    ("DELETE", "/debug/identities/aa/grants/bb/cc"),
    ("GET", "/debug/data/aa"),
    ("PUT", "/debug/data/aa/contact/email"),
    ("GET", "/debug/data/aa/contact/email"),
];

/// Close the endpoint. `shutdown` needs no exclusive ownership, but skipping
/// it anyway leaves the endpoint and every task a created identity spawned
/// running while the process tries to exit, which under load holds the exit
/// for minutes.
async fn shutdown(runtime: Arc<Runtime>, app: Router) -> Result<()> {
    drop(app);
    runtime.shutdown().await
}

#[tokio::test(flavor = "multi_thread")]
async fn live_is_200_and_debug_is_absent_without_the_flag() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), false);

    let live = app
        .clone()
        .oneshot(Request::get("/live").body(Body::empty())?)
        .await?;
    assert_eq!(live.status(), StatusCode::OK);
    let ready = app
        .clone()
        .oneshot(Request::get("/ready").body(Body::empty())?)
        .await?;
    assert_eq!(ready.status(), StatusCode::OK);

    // Paired deny: without the flag not one `/debug/` route exists.
    for (method, path) in DEBUG_ROUTES {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(*method)
                    .uri(*path)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} must be absent without the flag"
        );
    }

    shutdown(runtime, app).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn live_stays_up_while_ready_times_out_on_the_state_lock() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), false);
    let lock_holder = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            runtime
                .hold_state_lock_for_test(Duration::from_secs(3))
                .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let live = app
        .clone()
        .oneshot(Request::get("/live").body(Body::empty())?)
        .await?;
    assert_eq!(live.status(), StatusCode::OK);
    let ready = app
        .clone()
        .oneshot(Request::get("/ready").body(Body::empty())?)
        .await?;
    assert_eq!(ready.status(), StatusCode::INTERNAL_SERVER_ERROR);

    lock_holder.await?;
    shutdown(runtime, app).await?;
    Ok(())
}

/// The same routes answer something other than 404 with the flag on — so the
/// list above is a list of real routes, not of typos that would pass the
/// gate assertion for the wrong reason. Some of them (`/debug/status`,
/// `POST /debug/identities`, `GET /debug/identities`) carry no identifier to
/// malform and legitimately answer 200 on a fresh runtime; the rest answer a
/// client error. A served route is neither, so the one property common to
/// all of them — and the one a typo'd, unregistered route would fail — is
/// simply not being absent.
#[tokio::test(flavor = "multi_thread")]
async fn every_gated_route_exists_with_the_flag() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), true);

    for (method, path) in DEBUG_ROUTES {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(*method)
                    .uri(*path)
                    .body(Body::empty())?,
            )
            .await?;
        assert!(
            response.status() != StatusCode::NOT_FOUND,
            "{method} {path} must be a served route when the flag is on, got {}",
            response.status()
        );
    }

    shutdown(runtime, app).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_body_is_refused_before_its_handler() -> Result<()> {
    const MAX_BODY: usize = 16 * 1024 * 1024;

    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), true);
    let response = app
        .clone()
        .oneshot(Request::post("/debug/link").body(Body::from(vec![0_u8; MAX_BODY + 1]))?)
        .await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    shutdown(runtime, app).await?;
    Ok(())
}
