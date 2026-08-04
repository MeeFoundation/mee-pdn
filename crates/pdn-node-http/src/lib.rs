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

use std::sync::Arc;

use axum::{
    extract::State,
    routing::{delete, get, post, put},
    Router,
};
use pdn_node::{Runtime, SyncService as _};

pub use crate::{
    bind::{bind_addr, bind_addr_from_env, DEFAULT_HOST, DEFAULT_PORT},
    error::HostError,
};

/// Build the host's router over the embedded runtime. The scaffolding
/// routes exist only when `debug` is set — this one branch gates the whole
/// `/debug/` subtree, so off means absent and requests there fall through
/// to 404, not to an unauthorized answer.
pub fn router(runtime: Arc<Runtime>, debug: bool) -> Router {
    let app = Router::new().route("/live", get(live));
    let app = if debug {
        app.merge(debug_routes())
    } else {
        app
    };
    app.with_state(runtime)
}

/// Liveness: the process is up with its embedded runtime.
async fn live() -> &'static str {
    "ok"
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
async fn debug_status(State(runtime): State<Arc<Runtime>>) -> Result<String, HostError> {
    let sync = runtime.sync();
    let hosted = sync.hosted_identities().await?;
    let mut lines = vec![format!("node {}", sync.node_id())];
    lines.extend(
        hosted
            .into_iter()
            .map(|identity| format!("hosts {identity}")),
    );
    Ok(lines.join("\n") + "\n")
}
