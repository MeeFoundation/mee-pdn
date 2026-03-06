use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mee_node_api::{
    Contact, IdentityService as _, Invite, Node as _, SyncService as _, TrustService as _,
};
use mee_node_demo_impl::DemoNode;
use mee_sync_api as api;
use mee_sync_api::{AccessMode, SyncError};
use mee_sync_iroh_willow::DiscoveryConfig;
use mee_types::Did;
use serde::{Deserialize, Serialize};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct AppState {
    node: Arc<Mutex<Option<Arc<DemoNode>>>>,
    events: Arc<Mutex<Vec<String>>>,
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
            _ => DiscoveryConfig::disabled(),
        };
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let discovery: Arc<str> = std::env::var("MEE_DISCOVERY")
        .unwrap_or_else(|_| "disabled".into())
        .into();
    let state = AppState {
        node: Arc::new(Mutex::new(None)),
        events: Arc::new(Mutex::new(Vec::with_capacity(256))),
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
            "/p2p/ticket-by-did",
            post(|state, payload| async move { p2p_ticket_by_did(state, payload).await }),
        )
        .route(
            "/p2p/user-did",
            get(|state| async move { p2p_user_did(state).await }),
        )
        .route(
            "/p2p/validate-did",
            post(|state, payload| async move { p2p_validate_did(state, payload).await }),
        )
        .route(
            "/p2p/identity",
            post(|state, payload| async move { p2p_create_identity(state, payload).await }),
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
        )
        .route(
            "/p2p/events",
            get(|state| async move { p2p_events(state).await }),
        )
        .with_state(state);

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
struct UserDidResp {
    user_did: String,
}

async fn p2p_user_did(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let did = n.identity().current();
    Json(UserDidResp { user_did: did.0 }).into_response()
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
        did: invite.inviter_did.clone(),
        alias: None,
    });
    // Connect P2P: delegates capabilities and sends ticket
    // directly to peer
    match n
        .sync()
        .connect_to_peer(&invite.subspace_id, &invite.node, access)
        .await
    {
        Ok(()) => "connected".to_owned().into_response(),
        Err(e) => internal(&e).into_response(),
    }
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
        did: req.invite.inviter_did,
        alias: None,
    });
    "bound".to_owned().into_response()
}

#[derive(Deserialize)]
struct TicketByDidReq {
    did: Did,
    #[serde(default)]
    access: Option<AccessMode>,
}

async fn p2p_ticket_by_did(
    state: axum::extract::State<AppState>,
    Json(req): Json<TicketByDidReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    let Some(invite) = trust.invite_for(&req.did) else {
        return internal_str("did not bound; import invite first").into_response();
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
    let path = match api::EntryPath::new(req.path) {
        Ok(p) => p,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid path: {e}")).into_response();
        }
    };
    let n = state.get_node();
    match n.sync().insert(&path, req.body.as_bytes()).await {
        Ok(()) => "ok".to_owned().into_response(),
        Err(e) => internal(&e).into_response(),
    }
}

#[derive(Serialize)]
struct ListedEntry {
    subspace: String,
    path: String,
    payload_len: u64,
}

async fn p2p_list(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.sync().list().await {
        Ok(entries) => {
            let out: Vec<ListedEntry> = entries
                .into_iter()
                .map(|i| ListedEntry {
                    subspace: i.subspace.to_string(),
                    path: i.path.to_string(),
                    payload_len: i.payload_len,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => internal(&e).into_response(),
    }
}

#[allow(clippy::expect_used)]
async fn p2p_events(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    Json(state.events.lock().expect("events lock poisoned").clone()).into_response()
}

#[derive(Deserialize)]
struct ValidateDidReq {
    did: Did,
}

async fn p2p_validate_did(
    state: axum::extract::State<AppState>,
    Json(req): Json<ValidateDidReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    match n.identity().resolve(&req.did).await {
        Ok(doc) => Json(doc.id.0).into_response(),
        Err(e) => internal_str(&format!("did resolve error: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateIdentityReq {
    #[serde(default)]
    jwk: String,
    #[serde(default)]
    use_jcs_pub: bool,
}

#[derive(Serialize)]
struct CreateIdentityResp {
    did: String,
}

async fn p2p_create_identity(
    state: axum::extract::State<AppState>,
    Json(req): Json<CreateIdentityReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(&e).into_response();
    }
    let n = state.get_node();
    let params = mee_did_api::DidCreateParams::Key(mee_did_api::DidKeyCreateOptions {
        jwk: req.jwk,
        use_jcs_pub: req.use_jcs_pub,
    });
    match n.identity().create(&params).await {
        Ok(did) => Json(CreateIdentityResp { did: did.0 }).into_response(),
        Err(e) => internal_str(&format!("identity create error: {e}")).into_response(),
    }
}

fn internal(e: &SyncError) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn internal_str(msg: &str) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_owned())
}
