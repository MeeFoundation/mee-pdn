//! The concurrency ceiling on the `/debug/` subtree sheds the request past
//! it. Asserted through the real `router`, since the ceiling is a layer the
//! router attaches and only the assembled router shows it attached; and on
//! its own runtime rather than a container, since holding requests in flight
//! takes a held state lock, and no request on the surface can stall that
//! lock.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use pdn_node::{Runtime, SpawnOptions};
use pdn_node_http::{router, MAX_CONCURRENT_REQUESTS};
use tower::ServiceExt as _;

/// With the ceiling's worth of requests in flight, the one request past it
/// is shed with 503 and none of the others is. Each admitted request waits
/// out the host's two-second readiness budget on the held state lock, so
/// every request reaches admission long before the first admitted one
/// leaves.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_past_the_debug_ceiling_is_shed() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn(SpawnOptions::memory()).await?);
    let app = router(Arc::clone(&runtime), true);
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

    let requests: Vec<_> = (0..=MAX_CONCURRENT_REQUESTS)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                let request = Request::get("/debug/identities").body(Body::empty())?;
                let response = app.oneshot(request).await?;
                anyhow::Ok(response.status())
            })
        })
        .collect();
    let mut statuses = Vec::with_capacity(requests.len());
    for request in requests {
        statuses.push(request.await??);
    }
    release.notify_one();
    lock_holder.await?;

    let shed = statuses
        .iter()
        .filter(|status| **status == StatusCode::SERVICE_UNAVAILABLE)
        .count();
    assert_eq!(
        shed, 1,
        "exactly the request past the ceiling must be shed: {statuses:?}"
    );

    drop(app);
    runtime.shutdown().await
}
