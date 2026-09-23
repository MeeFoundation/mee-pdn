//! `/live` answers while `/ready` reports a held state lock — the one
//! property a node in a container cannot be made to show, since no request
//! on the surface can stall that lock. The only test that builds its own
//! runtime and router.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use pdn_node::{Runtime, SpawnOptions};
use pdn_node_http::router;
use tower::ServiceExt as _;

/// With the runtime's state lock held, `/live` answers 200 and `/ready`
/// reports the wait as 500 — a liveness probe must not kill a node whose
/// state is momentarily busy.
#[tokio::test(flavor = "multi_thread")]
async fn live_stays_up_while_ready_times_out_on_the_state_lock() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn(SpawnOptions::memory()).await?);
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
        .context("state-lock identity did not acquire the lock")?;

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
    drop(app);
    runtime.shutdown().await
}
