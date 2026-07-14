//! The HTTP host for the demo stand: a thin layer serving one embedded
//! [`pdn_node::Runtime`] over HTTP.
//!
//! One process, one embedded runtime. The HTTP surface is a host over the
//! core, not the platform API — other hosts (mobile, wasm) embed the same
//! core later, and the runtime itself stays host-free. The host adds no
//! authorization of its own: access to data remains bounded by ticket
//! possession, the embedded runtime's interim posture.
//!
//! `GET /live` is the one always-on route, probed by container harnesses
//! and the demo stand. Everything under `/debug/` is demo scaffolding:
//! absent unless `PDN_DEBUG=1` is set at startup, shape deliberately
//! unpinned and free to change without a spec change.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use pdn_node::{Runtime, SyncService as _};

/// Build the host's router over the embedded runtime. Debug scaffolding
/// routes exist only when `debug` is set: off means absent, so requests
/// under `/debug/` fall through to 404.
pub fn router(runtime: Arc<Runtime>, debug: bool) -> Router {
    let app = Router::new().route("/live", get(live));
    let app = if debug {
        app.route("/debug/status", get(debug_status))
    } else {
        app
    };
    app.with_state(runtime)
}

/// Liveness: the process is up with its embedded runtime.
async fn live() -> &'static str {
    "ok"
}

/// Demo scaffolding, shape unpinned: the node id and hosted identities.
async fn debug_status(State(runtime): State<Arc<Runtime>>) -> Result<String, (StatusCode, String)> {
    let sync = runtime.sync();
    let hosted = sync
        .hosted_identities()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut lines = vec![format!("node {}", sync.node_id())];
    lines.extend(
        hosted
            .into_iter()
            .map(|identity| format!("hosts {identity}")),
    );
    Ok(lines.join("\n") + "\n")
}
