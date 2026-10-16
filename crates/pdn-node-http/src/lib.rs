//! The HTTP host for the demo stand: a thin layer serving one embedded
//! [`pdn_node::Runtime`] over HTTP. Each route delegates to a single
//! service call; the host holds no state, authorizes nothing, and adds no
//! identity of its own. `PDN_DATA_DIR` is required — no in-memory mode, so
//! a host cannot promise persistence it does not provide. `GET /live` is
//! the one always-on route; `/debug/` is scaffolding behind `PDN_DEBUG` set
//! to `1` or `true`, its route names unpinned. The surface carries live
//! ceremony secrets and authenticates nobody — hence the flag and the
//! loopback default bind, which the stand's image overrides because a node
//! in a container has to serve every interface. Off the product path: a
//! product host embeds the runtime in-process, and between nodes nothing
//! HTTP travels.

mod bind;
mod error;
mod identity;
#[cfg(feature = "observation")]
mod observation;
mod parse;
mod pods;
pub mod shapes;

use std::{future::Future, sync::Arc, time::Duration};

use axum::{
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
    routing::{get, post},
    Router,
};
use pdn_node::{Runtime, SyncService as _};
use tokio::sync::watch;

#[cfg(feature = "observation")]
pub use crate::observation::MAX_OPEN_STREAMS;
pub use crate::{
    bind::{
        bind_addr, bind_addr_from_env, connectivity, connectivity_from_env, data_dir,
        data_dir_from_env, debug_enabled, debug_enabled_from_env, pod_reconcile_interval,
        pod_reconcile_interval_from_env, spawn_options, DEFAULT_HOST, DEFAULT_PORT,
    },
    error::HostError,
};

/// Replaces axum's undocumented 2 MB default: comfortably above a demo
/// record, far short of `pdn-store`'s 1 GB wire ceiling. Past it, axum
/// answers 413 directly, outside the error table.
pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Requests the `/debug/` subtree serves at once; past it, 503.
pub const MAX_CONCURRENT_REQUESTS: usize = 16;
/// The response header naming the pod an observed join joined
/// (`observation` feature).
pub const POD_HEADER: &str = "pdn-pod";

/// How long [`serve`] waits for axum's graceful drain once `stop` resolves.
/// This budget and the runtime's shutdown after it together have to fit
/// inside a container runtime's default grace — SIGTERM, then SIGKILL about
/// 10 seconds later — or the drain is the first thing lost.
pub const GRACEFUL_DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Serve `runtime` on `listener` until `stop` resolves, then drain for at
/// most [`GRACEFUL_DRAIN_BUDGET`]. Open event streams end at `stop`.
pub async fn serve(
    runtime: &Arc<Runtime>,
    listener: tokio::net::TcpListener,
    debug: bool,
    stop: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (stopped, mut stopping) = watch::channel(false);
    let app = assemble(Arc::clone(runtime), debug, stopping.clone());
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop.await;
        let _ = stopped.send(true);
    });
    let drain_budget_spent = async {
        if stopping.wait_for(|stopped| *stopped).await.is_err() {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(GRACEFUL_DRAIN_BUDGET).await;
    };
    tokio::select! {
        served = serve => served.map_err(Into::into),
        () = drain_budget_spent => {
            tracing::warn!(?GRACEFUL_DRAIN_BUDGET, "HTTP graceful-drain budget expired");
            Ok(())
        },
    }
}

/// This one branch gates the whole `/debug/` subtree: off means absent, so
/// requests there fall through to 404, not to an unauthorized answer. Event
/// streams of a router built here end only with the runtime.
pub fn router(runtime: Arc<Runtime>, debug: bool) -> Router {
    assemble(runtime, debug, watch::channel(false).1)
}

fn assemble(runtime: Arc<Runtime>, debug: bool, stopping: watch::Receiver<bool>) -> Router {
    let app = Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready));
    let app = if debug {
        let routes = debug_routes();
        #[cfg(feature = "observation")]
        let routes = routes.merge(observation::routes(Arc::clone(&runtime), stopping));
        #[cfg(not(feature = "observation"))]
        drop(stopping);
        let debug = routes
            .layer(middleware::from_fn_with_state(
                Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
                admit_request,
            ))
            .layer(middleware::from_fn(permit_any_origin));
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
/// replica and blob stores refusing every operation while the in-memory
/// bookkeeping keeps reporting a healthy node.
async fn ready(State(runtime): State<Arc<Runtime>>) -> Result<&'static str, HostError> {
    with_runtime_budget(runtime.sync().hosted_identities()).await?;
    with_runtime_budget(runtime.sync().check_storage()).await?;
    Ok("ok")
}

/// Cross-origin headers, so a page a browser loaded from somewhere else can
/// drive this node the way an application drives its own. Scoped to the
/// debug subtree, which exists only where `PDN_DEBUG` says so, and open to
/// any origin because a stand that answers on loopback has no origin worth
/// naming.
async fn permit_any_origin(request: Request, next: Next) -> Response {
    let preflight = request.method() == axum::http::Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("content-type"),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET,POST,PUT,DELETE,OPTIONS"),
    );
    response
}

/// One route to one service call. Deliberately absent, and to stay absent:
/// a pod store's ticket or a raw entry written into it (a path the
/// runtime's own callers lack; forgeries are the data layer's tests),
/// anything that forces a reconciliation (waiting is repeating the read),
/// anything that resets state, and any handler addressing another host.
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
            "/debug/identities/{identity}/pods",
            post(pods::create).get(pods::list),
        )
        .route("/debug/identities/{identity}/pods/join", post(pods::join))
        .route(
            "/debug/identities/{identity}/pods/{pod}/members",
            get(pods::members),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/invites",
            post(pods::invite),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/acts",
            post(pods::act),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/records",
            post(pods::put_record).get(pods::list_records),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/records/{member}/{kind}/{id}",
            get(pods::read),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/records/{member}/{kind}/{id}/ops",
            post(pods::append_op).get(pods::read_ops),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/unknown",
            get(pods::list_unknown),
        )
}

/// The one human-readable probe; the demo scripts lean on it.
async fn debug_status(
    State(runtime): State<Arc<Runtime>>,
    Query(shapes::NoQuery {}): Query<shapes::NoQuery>,
) -> Result<String, HostError> {
    let sync = runtime.sync();
    let hosted = with_runtime_budget(sync.hosted_identities()).await?;
    let mut lines = vec![format!("node {}", sync.node_id())];
    lines.extend(build_lines().map(str::to_owned));
    lines.extend(
        hosted
            .into_iter()
            .map(|identity| format!("hosts {identity}")),
    );
    Ok(lines.join("\n") + "\n")
}

/// Which build answers: `profile` from whether debug assertions are
/// compiled in, so a caller can refuse to measure the wrong one.
fn build_lines() -> [&'static str; 2] {
    [
        if cfg!(debug_assertions) {
            "profile debug"
        } else {
            "profile release"
        },
        if cfg!(feature = "observation") {
            "observation on"
        } else {
            "observation off"
        },
    ]
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

    /// A debug build states `profile debug`, and the observation line
    /// follows the feature it was built with.
    #[cfg(debug_assertions)]
    #[test]
    fn a_debug_build_states_its_profile() {
        let observation = if cfg!(feature = "observation") {
            "observation on"
        } else {
            "observation off"
        };
        assert_eq!(build_lines(), ["profile debug", observation]);
    }

    /// A request past the admission limit is shed with 503. The admission
    /// function alone, on a router of its own; that `router` attaches it is
    /// `tests/admission.rs`.
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
