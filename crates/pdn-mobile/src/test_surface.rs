//! The test build's surface: the laptop host's debug and observation routes on
//! the phone's loopback, beside a reset, the device's samples and its clock, so
//! a coordinator drives the phone's node over the cable as it drives a laptop
//! node (the `pdn-mobile/host` and `mobile-common/sync-test-agent` specs).
//! Absent from what ships.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Query, Request, State as Extract},
    http::{header::CONTENT_TYPE, StatusCode},
    response::{IntoResponse as _, Response},
    routing::{get, post},
    Json, Router,
};
use pdn_node::{IdentityService as _, Runtime, SyncService as _};
use serde::Deserialize;
use tower::ServiceExt as _;

use crate::node::{claimed, fresh_bring_up, install, State};

/// A day of samples at one a second; past it the oldest go first.
const SAMPLES_KEPT: usize = 86_400;

/// What a handle keeps for its surface, served or not.
#[derive(Default)]
pub(crate) struct Shared {
    samples: Mutex<Samples>,
    /// The port bound, once the surface is served.
    served: Mutex<Option<u16>>,
}

#[derive(Default)]
struct Samples {
    kept: VecDeque<(u64, String)>,
    next: u64,
}

impl Shared {
    /// Keep one sample, numbered in arrival order. It is checked to be JSON,
    /// kept on one line for the samples route, and otherwise read for nothing.
    pub(crate) fn report(&self, sample: &str) -> anyhow::Result<()> {
        let sample = serde_json::from_str::<serde_json::Value>(sample)?.to_string();
        let mut samples = self.samples.lock().map_err(|_| poisoned())?;
        let seq = samples.next;
        samples.next += 1;
        samples.kept.push_back((seq, sample));
        if samples.kept.len() > SAMPLES_KEPT {
            samples.kept.pop_front();
        }
        Ok(())
    }
}

/// Serve the surface on `127.0.0.1:port` (`0` picks a port) on the caller's
/// runtime, answering the port bound; a surface already served answers its
/// own port.
pub(crate) fn serve(
    shared: Arc<Shared>,
    state: Arc<Mutex<State>>,
    storage: PathBuf,
    port: u16,
) -> anyhow::Result<u16> {
    // Bound under the lock, so 2 calls at once serve once.
    let listener = {
        let mut served = shared.served.lock().map_err(|_| poisoned())?;
        if let Some(bound) = *served {
            return Ok(bound);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        *served = Some(listener.local_addr()?.port());
        listener
    };
    let bound = listener.local_addr()?.port();
    let surface = Arc::new(Surface {
        shared,
        state,
        storage,
        router: Mutex::new(None),
    });
    let shared = Arc::clone(&surface.shared);
    let app = Router::new()
        .route("/debug/agent/reset", post(reset))
        .route("/debug/agent/samples", get(samples))
        .route("/debug/agent/clock", get(clock))
        .fallback(forward)
        .layer(axum::middleware::from_fn(pdn_node_http::admit_local_host))
        .with_state(surface);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!("the test surface stopped serving: {err}");
        }
        // Served no more: the next call binds again.
        if let Ok(mut served) = shared.served.lock() {
            *served = None;
        }
    });
    tracing::info!("the test surface serves 127.0.0.1:{bound}");
    Ok(bound)
}

struct Surface {
    shared: Arc<Shared>,
    state: Arc<Mutex<State>>,
    storage: PathBuf,
    /// The laptop host's router for the node that is up, built once per node
    /// so its request and stream bounds hold across requests. Without the
    /// cross-origin headers: a page in the phone's browser is not a
    /// coordinator.
    router: Mutex<Option<(Arc<Runtime>, Router)>>,
}

impl Surface {
    /// `None` while no node is up.
    fn router(&self) -> Option<Router> {
        let runtime = match &*self.state.lock().ok()? {
            State::Up(runtime) => Arc::clone(runtime),
            State::Down | State::ComingUp(_) => return None,
        };
        let mut cached = self.router.lock().ok()?;
        if let Some((held, router)) = cached.as_ref() {
            if Arc::ptr_eq(held, &runtime) {
                return Some(router.clone());
            }
        }
        let router = pdn_node_http::router(Arc::clone(&runtime), true, false);
        *cached = Some((runtime, router.clone()));
        Some(router)
    }

    /// Stop the node, delete its directory, bring one up at `connectivity`
    /// and `pod_interval`, and create one identity. `None` while another
    /// bring-up is under way.
    async fn reset(
        &self,
        connectivity: pdn_node::Connectivity,
        pod_interval: Duration,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let (ours, _held) = fresh_bring_up()?;
        let previous = {
            let mut state = self.state.lock().map_err(|_| poisoned())?;
            if matches!(*state, State::ComingUp(_)) {
                return Ok(None);
            }
            match std::mem::replace(&mut *state, State::ComingUp(Arc::clone(&ours))) {
                State::Up(runtime) => Some(runtime),
                State::Down | State::ComingUp(_) => None,
            }
        };
        self.router.lock().map_err(|_| poisoned())?.take();
        let cleared = async {
            if let Some(runtime) = previous {
                runtime.shutdown().await?;
            }
            match std::fs::remove_dir_all(&self.storage) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
                _ => {}
            }
            std::fs::create_dir_all(&self.storage)?;
            anyhow::Ok(())
        }
        .await;
        if let Err(err) = cleared {
            if let Ok(mut state) = self.state.lock() {
                if claimed(&state, &ours) {
                    *state = State::Down;
                }
            }
            return Err(err);
        }
        // The shipping constructor, so the node under test is built as the
        // phone's is; the reconcile interval `bring_up` makes an application
        // name stays the runtime's default, which the laptop nodes run at.
        let options = pdn_node::SpawnOptions {
            connectivity,
            pod_reconcile_interval: pod_interval,
            ..pdn_node::SpawnOptions::for_product(self.storage.clone())
        };
        // The identity comes before the install: a reset that follows takes the
        // node only once this one has stopped writing to its directory.
        let created = async {
            let runtime = Runtime::spawn(options).await?;
            match runtime.identity().create().await {
                Ok(identity) => Ok((runtime, identity)),
                Err(err) => {
                    runtime.shutdown().await?;
                    Err(err)
                }
            }
        }
        .await;
        let (spawned, identity) = match created {
            Ok((runtime, identity)) => (Ok(runtime), Some(identity)),
            Err(err) => (Err(err), None),
        };
        if !install(&self.state, &ours, spawned).await? {
            anyhow::bail!("a stop overtook the reset");
        }
        let runtime = match &*self.state.lock().map_err(|_| poisoned())? {
            State::Up(runtime) => Arc::clone(runtime),
            State::Down | State::ComingUp(_) => anyhow::bail!("a stop overtook the reset"),
        };
        let identity =
            identity.ok_or_else(|| anyhow::anyhow!("a node came up without its identity"))?;
        Ok(Some(serde_json::json!({
            "identity": identity.to_string(),
            "node": runtime.sync().node_id().to_string(),
            "profile": profile(),
        })))
    }
}

#[derive(Deserialize)]
struct ResetRequest {
    pod_interval_secs: u64,
    /// As `PDN_CONNECTIVITY` names it: `direct`, `relays` or `product`.
    connectivity: String,
}

/// `Json`, which refuses any other content type: a page in the phone's
/// browser can send a form cross-origin without being asked, and that is
/// not a request to wipe the node.
async fn reset(
    Extract(surface): Extract<Arc<Surface>>,
    Json(request): Json<ResetRequest>,
) -> Response {
    if request.pod_interval_secs == 0 {
        return malformed("the pod interval is a positive whole number of seconds".to_owned());
    }
    let connectivity = match pdn_node_http::connectivity(Some(&request.connectivity)) {
        Ok(connectivity) => connectivity,
        Err(err) => return malformed(err.to_string()),
    };
    let pod_interval = Duration::from_secs(request.pod_interval_secs);
    // A task of its own: a client gone mid-reset drops this handler, and a
    // reset dropped between its stop and its bring-up would leave the handle
    // coming up for good.
    let resetting = tokio::spawn(async move { surface.reset(connectivity, pod_interval).await });
    match resetting
        .await
        .map_err(anyhow::Error::from)
        .and_then(|reset| reset)
    {
        Ok(Some(answer)) => Json(answer).into_response(),
        Ok(None) => (StatusCode::CONFLICT, "a bring-up is under way").into_response(),
        Err(err) => {
            tracing::error!("the reset failed: {err:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the reset failed: {err:#}"),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct After {
    after: Option<u64>,
}

/// One JSON line per sample after `after`: `{"seq":…,"sample":…}`, the
/// sample as the native module wrote it. A read after `after` releases the
/// samples up to it, which the phone's memory would otherwise carry.
async fn samples(Extract(surface): Extract<Arc<Surface>>, Query(after): Query<After>) -> Response {
    let Ok(mut samples) = surface.shared.samples.lock() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the sample lock is poisoned",
        )
            .into_response();
    };
    if let Some(after) = after.after {
        while samples.kept.front().is_some_and(|(seq, _)| *seq <= after) {
            samples.kept.pop_front();
        }
    }
    let mut lines = String::new();
    for (seq, sample) in samples
        .kept
        .iter()
        .filter(|(seq, _)| after.after.is_none_or(|after| *seq > after))
    {
        // Writing to a `String` cannot fail.
        let _ = writeln!(lines, "{{\"seq\":{seq},\"sample\":{sample}}}");
    }
    ([(CONTENT_TYPE, "application/x-ndjson")], lines).into_response()
}

/// The device's wall clock, for the coordinator's clock offset.
async fn clock() -> Json<serde_json::Value> {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        });
    Json(serde_json::json!({ "t": t }))
}

/// Every other route is the laptop host's, against the node that is up.
async fn forward(Extract(surface): Extract<Arc<Surface>>, request: Request) -> Response {
    match surface.router() {
        Some(router) => match router.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        None => (StatusCode::SERVICE_UNAVAILABLE, "no node is up").into_response(),
    }
}

fn malformed(what: String) -> Response {
    (StatusCode::BAD_REQUEST, what).into_response()
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn poisoned() -> anyhow::Error {
    anyhow::anyhow!("a test surface lock is poisoned")
}
