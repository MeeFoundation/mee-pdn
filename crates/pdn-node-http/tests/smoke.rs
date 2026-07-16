//! Host smoke tests: liveness while the embedded runtime runs, and the
//! debug gate — off means absent. The debug routes themselves are demo
//! scaffolding with an unpinned shape, so only the gate is asserted, not
//! their existence or form.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use pdn_node::Runtime;
use pdn_node_http::router;
use tower::ServiceExt as _;

#[tokio::test(flavor = "multi_thread")]
async fn live_is_200_and_debug_is_absent_without_the_flag() -> Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);
    let app = router(Arc::clone(&runtime), false);

    let live = app
        .clone()
        .oneshot(Request::get("/live").body(Body::empty())?)
        .await?;
    assert_eq!(live.status(), StatusCode::OK);

    // Paired deny: without the flag no `/debug/` route exists at all.
    let debug = app
        .oneshot(Request::get("/debug/status").body(Body::empty())?)
        .await?;
    assert_eq!(debug.status(), StatusCode::NOT_FOUND);

    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        runtime.shutdown().await?;
    }
    Ok(())
}
