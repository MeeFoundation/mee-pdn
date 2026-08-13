//! Liveness and readiness while the runtime's coarse state lock is held.
//!
//! The one property of this host that a node in a container cannot be made
//! to show: holding that lock is done from inside the runtime's own process,
//! and no request on the surface can stall it — every call the readiness
//! budget guards is, by design, never held across I/O. So this test builds
//! its runtime and its router here, and it is the only one that does.
//!
//! What it pins is the split: a container runtime's liveness probe must not
//! kill a node whose state is momentarily busy, so `/live` answers while
//! `/ready` reports the wait.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use pdn_node::Runtime;
use pdn_node_http::router;
use tower::ServiceExt as _;

#[tokio::test(flavor = "multi_thread")]
async fn live_stays_up_while_ready_times_out_on_the_state_lock() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), false);
    let acquired = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let lock_holder = {
        let runtime = Arc::clone(&runtime);
        let acquired = Arc::clone(&acquired);
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            runtime.hold_state_lock_for_test(acquired, release).await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), acquired.notified())
        .await
        .context("state-lock holder did not acquire the lock")?;

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

    release.notify_one();
    lock_holder.await?;
    // Skipping shutdown leaves the endpoint and every task a created
    // identity spawned running while the process tries to exit, which under
    // load holds the exit for minutes.
    drop(app);
    runtime.shutdown().await
}
