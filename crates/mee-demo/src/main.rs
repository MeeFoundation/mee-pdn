use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mee_node_api::{
    Contact, DataService as _, IdentityService as _, Invite, Node as _, SyncService as _,
    TrustService as _,
};
use mee_node_demo_impl::DemoNode;
use mee_sync_api as api;
use mee_sync_api::{AccessMode, SyncError};
use mee_sync_iroh_willow::gossip::GossipConfig;
use mee_sync_iroh_willow::DiscoveryConfig;
use mee_types::Aid;
use serde::{Deserialize, Serialize};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct AppState {
    node: Arc<Mutex<Option<Arc<DemoNode>>>>,
    discovery: Arc<str>,
}

#[allow(clippy::expect_used)]
impl AppState {
    async fn ensure(&self) -> Result<(), SyncError> {
        if self.node.lock().expect("node lock poisoned").is_some() {
            return Ok(());
        }
        let config = match &*self.discovery {
            "local" => DiscoveryConfig::local(),
            "full" => DiscoveryConfig::full(),
            "gossip" => {
                let mut gc = GossipConfig::default_config();
                if let Some(s) = std::env::var("MEE_GOSSIP_REBROADCAST_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    gc.rebroadcast_interval = Duration::from_secs(s);
                }
                if let Some(s) = std::env::var("MEE_GOSSIP_EVICTION_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    gc.eviction_threshold = Duration::from_secs(s);
                    gc.staleness_threshold = Duration::from_secs(s);
                }
                let mut config = DiscoveryConfig::disabled();
                config.gossip = Some(gc);
                config
            }
            _ => DiscoveryConfig::disabled(),
        };
        // TODO(persistent-storage): Read MEE_DATA_DIR env var and pass to
        // DemoNode::spawn(). Default to ./var/data/{port}/.
        let node = DemoNode::spawn(config)
            .await
            .map_err(|e| SyncError::Other(e.to_string()))?;
        *self.node.lock().expect("node lock poisoned") = Some(node);
        Ok(())
    }
    fn get_node(&self) -> Arc<DemoNode> {
        self.node
            .lock()
            .expect("node lock poisoned")
            .as_ref()
            .expect("get_node called before ensure")
            .clone()
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let discovery: Arc<str> = std::env::var("MEE_DISCOVERY")
        .unwrap_or_else(|_| "disabled".into())
        .into();
    let state = AppState {
        node: Arc::new(Mutex::new(None)),
        discovery,
    };

    let app = Router::new()
        .route("/live", get(|| async { "ok" }))
        .route(
            "/p2p/node",
            get(|state| async move { p2p_node(state).await }),
        )
        .route(
            "/p2p/subspace-id",
            get(|state| async move { p2p_subspace_id(state).await }),
        )
        .route(
            "/p2p/invite",
            get(|state| async move { p2p_invite(state).await }),
        )
        .route(
            "/p2p/connect",
            post(|state, payload| async move { p2p_connect(state, payload).await }),
        )
        .route(
            "/p2p/bind",
            post(|state, payload| async move { p2p_bind(state, payload).await }),
        )
        .route(
            "/p2p/ticket-by-aid",
            post(|state, payload| async move { p2p_ticket_by_aid(state, payload).await }),
        )
        .route(
            "/p2p/user-aid",
            get(|state| async move { p2p_user_aid(state).await }),
        )
        .route(
            "/p2p/validate-aid",
            post(|state, payload| async move { p2p_validate_aid(state, payload).await }),
        )
        .route(
            "/p2p/identity",
            post(|state| async move { p2p_create_identity(state).await }),
        )
        .route(
            "/p2p/ticket",
            post(|state, payload| async move { p2p_ticket(state, payload).await }),
        )
        .route(
            "/p2p/insert",
            post(|state, payload| async move { p2p_insert(state, payload).await }),
        )
        .route(
            "/p2p/list",
            get(|state| async move { p2p_list(state).await }),
        );

    // Debug/test routes — only available when MEE_DEBUG=1
    let app = if std::env::var("MEE_DEBUG").is_ok_and(|v| v == "1" || v == "true") {
        app.route(
            "/debug/endpoint-id",
            get(|state| async move { p2p_endpoint_id(state).await }),
        )
        .route(
            "/debug/import",
            post(|state, payload| async move { p2p_import(state, payload).await }),
        )
        .route(
            "/debug/gossip/peers",
            get(|state| async move { p2p_gossip_peers(state).await }),
        )
    } else {
        app
    };

    let app = app.with_state(state);

    let host = std::env::var("MEE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("MEE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3011);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn p2p_node(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.sync().node_addr().await {
        Ok(addr) => Json(addr).into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Serialize)]
struct SubspaceIdResp {
    subspace_id: String,
}

async fn p2p_subspace_id(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.sync().subspace_id().await {
        Ok(id) => Json(SubspaceIdResp {
            subspace_id: id.to_string(),
        })
        .into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Serialize)]
struct UserAidResp {
    user_aid: String,
}

async fn p2p_user_aid(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let aid = n.identity().aid();
    Json(UserAidResp {
        user_aid: aid.to_string(),
    })
    .into_response()
}

async fn p2p_invite(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.trust().create_invite().await {
        Ok(inv) => Json(inv).into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Deserialize)]
struct ConnectReq {
    invite: Invite,
    #[serde(default)]
    access: Option<AccessMode>,
}

async fn p2p_connect(
    state: axum::extract::State<AppState>,
    Json(req): Json<ConnectReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    let invite = req.invite;
    let access = req.access.unwrap_or(AccessMode::Read);
    // Remember the invite and contact before connecting
    trust.remember_invite(invite.clone());
    trust.add_contact(Contact {
        aid: invite.inviter_aid,
        alias: None,
    });
    // Try direct connection via each node_hint.
    for hint in &invite.node_hints {
        if n.sync()
            .connect_to_peer(&invite.subspace_id, hint, access)
            .await
            .is_ok()
        {
            return "connected".to_owned().into_response();
        }
    }

    // All hints failed (or none provided) — defer to gossip discovery.
    // Store a pending marker so the gossip event loop can match this
    // invite against future peer advertisements.
    let pending_key =
        mee_types::local_store::keys::pending_invite(&invite.subspace_id, &invite.namespace_id);
    let _ = n.store().set(&pending_key, Vec::new());
    "pending".to_owned().into_response()
}

#[derive(Deserialize)]
struct BindReq {
    invite: Invite,
}

async fn p2p_bind(state: axum::extract::State<AppState>, Json(req): Json<BindReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    trust.remember_invite(req.invite.clone());
    trust.add_contact(Contact {
        aid: req.invite.inviter_aid,
        alias: None,
    });
    "bound".to_owned().into_response()
}

#[derive(Deserialize)]
struct TicketByAidReq {
    aid: Aid,
    #[serde(default)]
    access: Option<AccessMode>,
}

async fn p2p_ticket_by_aid(
    state: axum::extract::State<AppState>,
    Json(req): Json<TicketByAidReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    let Some(invite) = trust.invite_for(&req.aid) else {
        return internal_str("aid not bound; import invite first").into_response();
    };
    let access = req.access.unwrap_or(AccessMode::Read);
    match trust.accept_invite(&invite, access).await {
        Ok(ticket) => Json(ticket).into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Deserialize)]
struct TicketReq {
    to_subspace: api::SubspaceId,
    #[serde(default)]
    access: Option<AccessMode>,
}

async fn p2p_ticket(state: axum::extract::State<AppState>, Json(req): Json<TicketReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let access = req.access.unwrap_or(AccessMode::Read);
    match n.sync().share(&req.to_subspace, access).await {
        Ok(ticket) => Json::<api::SyncTicket>(ticket).into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Deserialize)]
struct InsertReq {
    path: String,
    body: String,
}

async fn p2p_insert(state: axum::extract::State<AppState>, Json(req): Json<InsertReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.data().set(&req.path, &req.body).await {
        Ok(()) => "ok".to_owned().into_response(),
        Err(e) => internal_str(&e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
struct ListedEntry {
    key: String,
    value: String,
}

async fn p2p_list(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.data().list("").await {
        Ok(entries) => {
            let out: Vec<ListedEntry> = entries
                .into_iter()
                .map(|i| ListedEntry {
                    key: i.key,
                    value: i.value,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => internal_str(&e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ValidateAidReq {
    aid: Aid,
}

async fn p2p_validate_aid(
    state: axum::extract::State<AppState>,
    Json(req): Json<ValidateAidReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.identity().resolve(&req.aid).await {
        Ok(identity_state) => Json(identity_state.aid.to_string()).into_response(),
        Err(e) => internal_str(&format!("identity resolve error: {e}")).into_response(),
    }
}

// TODO(keri): Add params for key type, witness config when real
// KERI inception is implemented. Currently parameterless.
#[derive(Serialize)]
struct CreateIdentityResp {
    aid: String,
}

async fn p2p_create_identity(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.identity().create().await {
        Ok(aid) => Json(CreateIdentityResp {
            aid: aid.to_string(),
        })
        .into_response(),
        Err(e) => internal_str(&format!("identity create error: {e}")).into_response(),
    }
}

// -- Gossip routes ----------------------------------------------------------

#[derive(Serialize)]
struct EndpointIdResp {
    endpoint_id: String,
}

async fn p2p_endpoint_id(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let eid = n.sync().core().endpoint().id();
    Json(EndpointIdResp {
        endpoint_id: hex::encode(eid.as_bytes()),
    })
    .into_response()
}

#[derive(Deserialize)]
struct ImportReq {
    ticket: api::SyncTicket,
}

async fn p2p_import(state: axum::extract::State<AppState>, Json(req): Json<ImportReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.sync().import(req.ticket, api::SyncMode::Continuous).await {
        Ok(_handle) => "imported".to_owned().into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Serialize)]
struct CachedPeerResp {
    peer_id: String,
    namespace_ids: Vec<String>,
    version: u64,
    connected: bool,
}

async fn p2p_gossip_peers(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let Some(gossip) = n.sync().core().gossip_manager() else {
        return internal_str("gossip not enabled").into_response();
    };
    match gossip.cached_peers().await {
        Ok(peers) => {
            let out: Vec<CachedPeerResp> = peers
                .into_iter()
                .map(|p| CachedPeerResp {
                    peer_id: hex::encode(p.peer_id),
                    namespace_ids: p.namespace_ids.iter().map(hex::encode).collect(),
                    version: p.version,
                    connected: p.connected,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => internal_str(&e.to_string()).into_response(),
    }
}

// -- Helpers ----------------------------------------------------------------

fn internal(e: &SyncError) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn internal_str(msg: &str) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_owned())
}
