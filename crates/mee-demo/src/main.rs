use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt as _;
use mee_node_api::{
    Contact, IdentityService as _, Invite, Node as _, SyncService as _,
    TrustService as _,
};
use mee_node_demo_impl::DemoNode;
use mee_sync_api as api;
use mee_sync_api::{SyncEngine, SyncError};
use mee_types::Did;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct AppState {
    node: Arc<Mutex<Option<Arc<DemoNode>>>>,
    events: Arc<Mutex<Vec<String>>>,
    sync_complete: Arc<Mutex<bool>>,
}

impl AppState {
    async fn ensure(&self) -> Result<(), SyncError> {
        if self.node.lock().unwrap().is_some() {
            return Ok(());
        }
        let node = DemoNode::spawn()
            .await
            .map_err(|e| SyncError::Other(e.to_string()))?;
        *self.node.lock().unwrap() = Some(node);
        Ok(())
    }
    fn get_node(&self) -> Arc<DemoNode> {
        self.node.lock().unwrap().as_ref().unwrap().clone()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = AppState {
        node: Arc::new(Mutex::new(None)),
        events: Arc::new(Mutex::new(Vec::with_capacity(256))),
        sync_complete: Arc::new(Mutex::new(false)),
    };

    let app = Router::new()
        .route("/live", get(|| async { "ok" }))
        .route(
            "/p2p/node",
            get(|state| async move { p2p_node(state).await }),
        )
        .route(
            "/p2p/transport-user-id",
            get(|state| async move { p2p_transport_user_id(state).await }),
        )
        .route(
            "/p2p/invite",
            get(|state| async move { p2p_invite(state).await }),
        )
        .route(
            "/p2p/ticket-from-invite",
            post(|state, payload| async move { p2p_ticket_from_invite(state, payload).await }),
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
            "/p2p/import",
            post(|state, payload| async move { p2p_import(state, payload).await }),
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
        .route(
            "/p2p/sync-status",
            get(|state, q| async move { p2p_sync_status(state, q).await }),
        )
        .with_state(state);

    let host = std::env::var("MEE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("MEE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3011);
    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn p2p_node(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.sync().node_addr().await {
        Ok(addr) => Json(addr).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Serialize)]
struct TransportUserIdResp {
    transport_user_id: String,
}

async fn p2p_transport_user_id(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.sync().user_id().await {
        Ok(u) => Json(TransportUserIdResp {
            transport_user_id: u.0,
        })
        .into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Serialize)]
struct UserDidResp {
    user_did: String,
}

async fn p2p_user_did(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    let did = n.identity().current();
    Json(UserDidResp { user_did: did.0 }).into_response()
}

async fn p2p_invite(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.trust().create_invite().await {
        Ok(inv) => Json(inv).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct TicketFromInviteReq {
    invite: Invite,
    #[serde(default)]
    write: bool,
}

async fn p2p_ticket_from_invite(
    state: axum::extract::State<AppState>,
    Json(req): Json<TicketFromInviteReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    let invite = req.invite;
    match trust.accept_invite(&invite, req.write).await {
        Ok(ticket) => {
            trust.remember_invite(invite.clone());
            trust.add_contact(Contact {
                did: invite.inviter_did,
                alias: None,
            });
            Json(ticket).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct BindReq {
    invite: Invite,
}

async fn p2p_bind(state: axum::extract::State<AppState>, Json(req): Json<BindReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    trust.remember_invite(req.invite.clone());
    trust.add_contact(Contact {
        did: req.invite.inviter_did,
        alias: None,
    });
    "bound".to_string().into_response()
}

#[derive(Deserialize)]
struct TicketByDidReq {
    did: Did,
    #[serde(default)]
    write: bool,
}

async fn p2p_ticket_by_did(
    state: axum::extract::State<AppState>,
    Json(req): Json<TicketByDidReq>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    let trust = n.trust();
    let invite = match trust.invite_for(&req.did) {
        Some(i) => i,
        None => return internal_str("did not bound; import invite first").into_response(),
    };
    // Verification is performed by ticket issuance.
    match trust.accept_invite(&invite, req.write).await {
        Ok(ticket) => Json(ticket).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

// time/sig helpers moved to mee-node-demo-impl; server delegates to node.

#[derive(Deserialize)]
struct TicketReq {
    to_user: api::TransportUserId,
    #[serde(default)]
    write: bool,
}

async fn p2p_ticket(state: axum::extract::State<AppState>, Json(req): Json<TicketReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.sync().share(&req.to_user, req.write).await {
        Ok(ticket) => Json::<api::SyncTicket>(ticket).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct ImportReq {
    ticket: api::SyncTicket,
    #[serde(default)]
    continuous: bool,
}

async fn p2p_import(state: axum::extract::State<AppState>, Json(req): Json<ImportReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    let mode = if req.continuous {
        api::SyncMode::Continuous
    } else {
        api::SyncMode::ReconcileOnce
    };
    let mut handle = match n.sync().import(req.ticket, mode).await {
        Ok(h) => h,
        Err(e) => return internal(e).into_response(),
    };
    let events = state.events.clone();
    let done = state.sync_complete.clone();
    tokio::spawn(async move {
        let mut h = Pin::new(&mut *handle);
        while let Some(ev) = h.next().await {
            let name = match ev {
                api::SyncEvent::CapabilityIntersection => "capability_intersection",
                api::SyncEvent::InterestIntersection => "interest_intersection",
                api::SyncEvent::Reconciled => "reconciled",
                api::SyncEvent::ReconciledAll => {
                    *done.lock().unwrap() = true;
                    "reconciled_all"
                }
                api::SyncEvent::Abort { .. } => "abort",
            };
            let mut buf = events.lock().unwrap();
            if buf.len() >= 500 {
                buf.remove(0);
            }
            buf.push(name.to_string());
        }
    });
    "imported".to_string().into_response()
}

#[derive(Deserialize)]
struct InsertReq {
    path: api::EntryPath,
    body: String,
}

async fn p2p_insert(state: axum::extract::State<AppState>, Json(req): Json<InsertReq>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n
        .sync()
        .insert(&req.path, req.body.as_bytes())
        .await
    {
        Ok(()) => "ok".to_string().into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Serialize)]
struct ListedEntry {
    subspace_hex: String,
    path: String,
    payload_len: u64,
}

async fn p2p_list(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.sync().list().await {
        Ok(entries) => {
            let out: Vec<ListedEntry> = entries
                .into_iter()
                .map(|i| ListedEntry {
                    subspace_hex: i.subspace_hex.to_string(),
                    path: i.path.to_string(),
                    payload_len: i.payload_len,
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

async fn p2p_events(state: axum::extract::State<AppState>) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    Json(state.events.lock().unwrap().clone()).into_response()
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
        return internal(e).into_response();
    }
    let n = state.get_node();
    match n.identity().resolve(&req.did).await {
        Ok(doc) => Json(doc.id.0).into_response(),
        Err(e) => internal_str(&format!("did resolve error: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct SyncStatusQuery {
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[derive(Serialize)]
struct SyncStatus {
    status: String,
}

async fn p2p_sync_status(
    state: axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SyncStatusQuery>,
) -> Response {
    if let Err(e) = state.ensure().await {
        return internal(e).into_response();
    }
    if let Some(ms) = q.wait_ms {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    let s = if *state.sync_complete.lock().unwrap() {
        "complete"
    } else {
        "pending"
    };
    Json(SyncStatus {
        status: s.to_string(),
    })
    .into_response()
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
        return internal(e).into_response();
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

fn internal(e: SyncError) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn internal_str(msg: &str) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string())
}
