//! The HTTP host for the demo stand: a thin layer serving one embedded
//! [`pdn_node::Runtime`] over HTTP.
//!
//! One process, one embedded runtime. The host holds no state and
//! orchestrates nothing: each route delegates to a single service call of
//! the runtime it embeds. It also authorizes nothing — every refusal is the
//! runtime's, on the runtime's terms — and it adds no identity of its own.
//!
//! `GET /live` is the one always-on route, probed by container harnesses
//! and the demo stand. Everything under `/debug/` is scaffolding for that
//! stand: absent unless `PDN_DEBUG=1` is set at startup, and its route
//! names, paths, and body shapes are free to change without a spec change.
//! Nothing outside this repository may depend on them.
//!
//! The surface carries live ceremony secrets. An invite payload and a
//! linking payload cross it in the clear, each with its one-time secret:
//! bearer-free in the ceremonies' sense — no ticket, no identity proof,
//! nothing granting durable access — yet live until burnt or expired, so
//! whoever captures one can consume the invitation in the intended
//! recipient's place. No namespace ticket crosses the surface at all. Hence
//! the flag, and the loopback default bind ([`bind_addr`]).
//!
//! **The host is off the product path.** It exists so a test can reach a
//! node from outside its process — the container stand and the demo are its
//! only intended deployments. A product host, mobile or desktop, embeds the
//! runtime core in-process; the runtime carries no HTTP dependency, and no
//! product path includes an HTTP endpoint. Between nodes nothing HTTP
//! travels either: the establishment and linking dialogues, reconciliation,
//! and gossip all run over the runtime's own protocols on its iroh
//! connections, so a scenario driven over this surface exercises the
//! product's inter-node path, with HTTP standing in for the in-process
//! method call and for nothing else.

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
        bind_addr, bind_addr_from_env, debug_enabled, debug_enabled_from_env, DEFAULT_HOST,
        DEFAULT_PORT,
    },
    error::HostError,
};

/// The request body ceiling for the whole router, replacing axum's
/// undocumented 2 MB default with a named, documented one: comfortably
/// above a realistic demo entry, far short of `pdn-store`'s own 1 GB wire
/// ceiling, so a caller pushing a production-sized entry through this
/// surface meets a deliberate demo-stand limit rather than the engine's.
/// Past it, axum answers 413 directly — outside the closed error table,
/// same as the two statuses [`HostError`] decides on its own.
const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 16;

/// Build the host's router over the embedded runtime. The scaffolding
/// routes exist only when `debug` is set — this one branch gates the whole
/// `/debug/` subtree, so off means absent and requests there fall through
/// to 404, not to an unauthorized answer.
pub fn router(runtime: Arc<Runtime>, debug: bool) -> Router {
    let app = Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready));
    let app = if debug {
        app.merge(debug_routes())
    } else {
        app
    };
    app.layer(middleware::from_fn_with_state(
        Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        admit_request,
    ))
    .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
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

/// Maximum wait for a readiness read of the coarse runtime lock.
const READINESS_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Apply the shared readiness budget to a runtime read.
pub(crate) async fn with_runtime_budget<T>(
    fut: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, HostError> {
    tokio::time::timeout(READINESS_BUDGET, fut)
        .await
        .map_err(|_elapsed| HostError::from(anyhow::anyhow!("runtime lock check timed out")))?
        .map_err(HostError::from)
}

/// Liveness reports that the host and its embedded runtime still exist.
async fn live(State(_runtime): State<Arc<Runtime>>) -> &'static str {
    "ok"
}

/// Readiness reports whether the runtime's state answers within its budget.
async fn ready(State(runtime): State<Arc<Runtime>>) -> Result<&'static str, HostError> {
    with_runtime_budget(runtime.sync().hosted_identities()).await?;
    Ok("ok")
}

/// The scaffolding subtree: the embedded runtime's service operations, one
/// route to one service call.
///
/// What is deliberately absent, and must stay absent — the reason is what a
/// test built on this surface proves, and both substitutions below sit in
/// arrange and act steps where nothing downstream reveals them, because the
/// assertions still read the right value, obtained the wrong way:
///
/// - **No namespace ticket handover** — neither the out-of-band share and
///   import, nor a ticket inside a grant read. The runtime binds what a
///   grant names by itself, so nothing here needs one; a harness that
///   arranged a granted namespace by importing its ticket would keep
///   passing after the grant binder broke.
/// - **Nothing that forces a reconciliation.** The first container test
///   that goes red waiting for convergence makes this the cheap fix. The
///   product converges on its own — a nudge before access, and the periodic
///   pass — and a forced sync in an act step would prove something the
///   stand does not do. Waiting is repeating the read, which is what an
///   application does and what fails for the true reason when convergence
///   never comes.
/// - **Nothing that resets state, writes into a store directly, or
///   fabricates a device record.** None of it exists for an embedder of the
///   runtime, so none of it exists here.
/// - **No handler addressing another host.** The crate carries no HTTP
///   client at all: everything between nodes travels the runtime's own
///   protocols, and a ceremony payload moves between containers through the
///   caller, as the product's payload moves between devices through a
///   human.
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
            "/debug/identities/{identity}/grants/{peer}/{issuer}",
            delete(connections::withdraw_grant),
        )
        .route("/debug/data/{issuer}", get(data::list))
        .route(
            "/debug/data/{issuer}/{*path}",
            put(data::write).get(data::read),
        )
}

/// The one human-readable probe: the node id and hosted identities. The
/// identity list above reports the same hosted set as JSON, and this stays
/// beside it because the demo script leans on it.
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

    /// The one piece of logic that decides `/live`'s (and `/debug/status`'s
    /// and `/debug/identities`'s) pass/fail on a stuck runtime lock: a
    /// future that outlives [`READINESS_BUDGET`] must answer non-200 within
    /// a bounded margin, not hang. Deliberately not an end-to-end HTTP
    /// test — there is no legitimate way to make the real coarse lock
    /// stall for the budget's duration through the public surface, since
    /// every runtime call this budget guards is, by design, never held
    /// across I/O. Wiring this into a non-200 response is covered by the
    /// existing `/live` success test (`tests/smoke.rs`) plus `error.rs`'s
    /// `HostError` unit tests.
    #[tokio::test]
    async fn a_stalled_runtime_call_times_out_within_its_budget() {
        let started = std::time::Instant::now();
        let result: Result<(), HostError> =
            with_runtime_budget(std::future::pending::<anyhow::Result<()>>()).await;
        let elapsed = started.elapsed();
        let err = result.expect_err("a pending future must time out, not resolve");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            elapsed < READINESS_BUDGET + std::time::Duration::from_secs(2),
            "with_runtime_budget took {elapsed:?} against a {READINESS_BUDGET:?} budget"
        );
    }

    #[tokio::test]
    async fn a_request_above_the_concurrency_limit_is_shed() {
        let app = Router::new()
            .route(
                "/",
                get(|| async { std::future::pending::<&'static str>().await }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
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
        tokio::task::yield_now().await;

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
