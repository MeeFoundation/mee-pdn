//! The HTTP host for the demo stand: a thin layer serving one embedded
//! [`pdn_node::Runtime`] over HTTP. Each route delegates to a single
//! service call; the host holds no state, authorizes nothing, and adds no
//! identity of its own. `PDN_DATA_DIR` is required — no in-memory mode, so
//! a host cannot promise persistence it does not provide. `GET /live` is
//! the one always-on route; `/debug/` is scaffolding behind `PDN_DEBUG=1`,
//! its route names unpinned. The surface carries live ceremony secrets and
//! authenticates nobody — hence the flag and the loopback default bind,
//! which the stand's image overrides because a node in a container has to
//! serve every interface. Off the product path: a product host embeds the
//! runtime in-process, and between nodes nothing HTTP travels.

mod bind;
mod connections;
mod data;
mod error;
mod identity;
mod parse;
pub mod shapes;

use std::{future::Future, sync::Arc};

use axum::{
    extract::{Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
    routing::{delete, get, post, put},
    Router,
};
use pdn_node::{Runtime, SyncService as _};

pub use crate::{
    bind::{
        bind_addr, bind_addr_from_env, data_dir, data_dir_from_env, debug_enabled,
        debug_enabled_from_env, DEFAULT_HOST, DEFAULT_PORT,
    },
    error::HostError,
};

/// Replaces axum's undocumented 2 MB default: comfortably above a demo
/// entry, far short of `pdn-store`'s 1 GB wire ceiling. Past it, axum
/// answers 413 directly, outside the error table.
pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 16;

/// This one branch gates the whole `/debug/` subtree: off means absent, so
/// requests there fall through to 404, not to an unauthorized answer.
pub fn router(runtime: Arc<Runtime>, debug: bool) -> Router {
    let app = Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready));
    let app = if debug {
        let debug = debug_routes().layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            admit_request,
        ));
        app.merge(debug)
    } else {
        app
    };
    app.layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(runtime)
}

async fn admit_request(
    State(limit): State<Arc<tokio::sync::Semaphore>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = limit.try_acquire_owned() else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    next.run(request).await
}

const READINESS_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) async fn with_runtime_budget<T>(
    fut: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, HostError> {
    tokio::time::timeout(READINESS_BUDGET, fut)
        .await
        .map_err(|_elapsed| HostError::from(anyhow::anyhow!("runtime lock check timed out")))?
        .map_err(HostError::from)
}

async fn live(State(_runtime): State<Arc<Runtime>>) -> &'static str {
    "ok"
}

/// The storage read is the half that can say no: a full disk leaves the
/// replica store refusing every operation while the in-memory bookkeeping
/// keeps reporting a healthy node.
async fn ready(State(runtime): State<Arc<Runtime>>) -> Result<&'static str, HostError> {
    with_runtime_budget(runtime.sync().hosted_identities()).await?;
    with_runtime_budget(runtime.sync().check_storage()).await?;
    Ok("ok")
}

/// One route to one service call. Deliberately absent, and to stay absent:
/// any namespace ticket handover (a harness that arranged a granted
/// namespace by importing its ticket would keep passing after the grant
/// binder broke), anything that forces a reconciliation (waiting is
/// repeating the read), anything that resets state, and any handler
/// addressing another host.
fn debug_routes() -> Router<Arc<Runtime>> {
    Router::new()
        .route("/debug/status", get(debug_status))
        .route(
            "/debug/identities",
            post(identity::create).get(identity::hosted),
        )
        .route(
            "/debug/identities/{identity}/linking-invite",
            post(identity::linking_invite),
        )
        .route("/debug/link", post(identity::link))
        .route(
            "/debug/identities/{identity}/invite",
            post(connections::invite),
        )
        .route(
            "/debug/identities/{identity}/establish",
            post(connections::establish),
        )
        .route(
            "/debug/identities/{identity}/connections",
            get(connections::list),
        )
        .route(
            "/debug/identities/{identity}/grants/{peer}",
            post(connections::publish_grant).get(connections::read_grants),
        )
        .route(
            "/debug/identities/{identity}/own-grants/{peer}",
            get(connections::read_own_grants),
        )
        .route(
            "/debug/identities/{identity}/grants/{peer}/{issuer}",
            delete(connections::withdraw_grant),
        )
        .route("/debug/data/{identity}/{issuer}", get(data::list))
        .route(
            "/debug/data/{identity}/{issuer}/{*path}",
            put(data::write).get(data::read),
        )
}

/// The one human-readable probe; the demo script leans on it.
async fn debug_status(
    State(runtime): State<Arc<Runtime>>,
    Query(shapes::NoQuery {}): Query<shapes::NoQuery>,
) -> Result<String, HostError> {
    let sync = runtime.sync();
    let hosted = with_runtime_budget(sync.hosted_identities()).await?;
    let mut lines = vec![format!("node {}", sync.node_id())];
    lines.extend(
        hosted
            .into_iter()
            .map(|identity| format!("hosts {identity}")),
    );
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt as _;

    use super::*;

    /// A future that outlives [`READINESS_BUDGET`] answers non-200 within a
    /// bounded margin, not a hang. Not end to end: the real coarse lock
    /// cannot be stalled through the public surface, since every call the
    /// budget guards is never held across I/O (`tests/readiness.rs` holds it
    /// from inside).
    #[tokio::test(start_paused = true)]
    async fn a_stalled_runtime_call_times_out_within_its_budget() {
        let started = tokio::time::Instant::now();
        let result: Result<(), HostError> =
            with_runtime_budget(std::future::pending::<anyhow::Result<()>>()).await;
        let elapsed = started.elapsed();
        let err = result.expect_err("a pending future must time out, not resolve");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(elapsed, READINESS_BUDGET);
    }

    #[tokio::test]
    async fn a_request_above_the_concurrency_limit_is_shed() {
        let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));
        let app = Router::new()
            .route(
                "/",
                get(|| async { std::future::pending::<&'static str>().await }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::clone(&limit),
                admit_request,
            ));
        let mut admitted = Vec::new();
        for _ in 0..MAX_CONCURRENT_REQUESTS {
            let service = app.clone();
            admitted.push(tokio::spawn(async move {
                service
                    .oneshot(Request::get("/").body(Body::empty()).expect("request"))
                    .await
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while limit.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all permits must be acquired");

        let shed = app
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);

        for task in admitted {
            task.abort();
        }
    }
}
