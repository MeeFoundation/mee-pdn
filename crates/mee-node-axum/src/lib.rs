use axum::{
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use mee_did_api::{DidCreateParams, DidKeyCreateOptions, DidProvider};
use mee_did_key::KeyDidManager;
use mee_node_api::Node;
use mee_transport_api::{Message, MessageKind, ProfileName, Session, Ticket, Transport};
use mee_transport_http::HttpTransport;
use mee_types::Did;
use mee_types::{NodeId, UserId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Clone)]
pub struct AxumNode {
    profile: ProfileName,
    node_id: NodeId,
    transport: HttpTransport,
    store: mee_local_store_mem::MemKvStore,
}

impl AxumNode {
    pub fn new(profile: ProfileName, base_url: String) -> Self {
        let did = Did::from("did:key:zaxum");
        Self {
            profile,
            node_id: NodeId::from(did.as_ref()),
            transport: HttpTransport::new(base_url),
            store: mee_local_store_mem::MemKvStore::new(),
        }
    }
}

impl mee_node_api::Node for AxumNode {
    type Transport = HttpTransport;
    type DidManager = mee_did_key::KeyDidManager;
    type Store = mee_local_store_mem::MemKvStore;

    fn profile(&self) -> &ProfileName {
        &self.profile
    }
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }
    fn user_id(&self) -> Option<&UserId> {
        None
    }
    fn transport(&self) -> &Self::Transport {
        &self.transport
    }
    fn did_manager(&self) -> &Self::DidManager {
        // zero-sized provider; return a reference to a static instance
        const KEY_MANAGER: mee_did_key::KeyDidManager = mee_did_key::KeyDidManager;
        &KEY_MANAGER
    }
    fn store(&self) -> &Self::Store {
        &self.store
    }
}

#[derive(Clone)]
pub struct AppState {
    pub node: AxumNode,
}

#[derive(Serialize)]
struct TicketResponse {
    ticket: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct InboxItem {
    from: String,
    kind: String,
    body_b64: String,
}

#[derive(Deserialize)]
struct IncomingMessage {
    from: String,
    kind: String,
    body_b64: String,
}

pub async fn build_app(profile: ProfileName, base_url: String) -> Router {
    let _did = KeyDidManager
        .create(&DidCreateParams::Key(DidKeyCreateOptions {
            jwk: String::new(),
            use_jcs_pub: false,
        }))
        .await
        .unwrap_or_else(|_| Did("did:key:zmem".into()));

    let state = AppState {
        node: AxumNode::new(profile.clone(), base_url),
    };

    Router::new()
        .route("/live", get(|| async { "ok" }))
        // Demo control plane
        .route("/demo/ticket", get(get_ticket))
        .route("/demo/inbox", get(list_inbox))
        .route("/demo/send/ping", post(send_ping))
        // inbox endpoint used by transports (temp)
        .route("/profiles/{name}/inbox", post(post_inbox))
        .with_state(state)
}

async fn get_ticket(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    match state.node.transport().ticket(state.node.profile()).await {
        Ok(t) => (
            StatusCode::OK,
            Json(TicketResponse {
                ticket: t.to_string(),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(TicketResponse {
                ticket: format!("error: {e}"),
            }),
        ),
    }
}

async fn list_inbox(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    use mee_local_store_api::{Key, KvStore, Namespace};
    let ns = Namespace("inbox".to_string());
    let key = Key("items".to_string());
    let list: Vec<InboxItem> = match state.node.store().get(&ns, &key) {
        Ok(Some(val)) => serde_json::from_str(&val.0).unwrap_or_default(),
        _ => Vec::new(),
    };
    (StatusCode::OK, Json(list))
}

async fn post_inbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<IncomingMessage>,
) -> impl IntoResponse {
    if name != state.node.profile().0 {
        return (StatusCode::NOT_FOUND, "wrong profile");
    }
    let body = match general_purpose::STANDARD.decode(payload.body_b64.as_bytes()) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid body"),
    };
    let item = InboxItem {
        from: payload.from,
        kind: payload.kind,
        body_b64: general_purpose::STANDARD.encode(&body),
    };
    use mee_local_store_api::{Key, KvStore, Namespace, Value};
    let ns = Namespace("inbox".to_string());
    let key = Key("items".to_string());
    let mut list: Vec<InboxItem> = match state.node.store().get(&ns, &key) {
        Ok(Some(val)) => serde_json::from_str(&val.0).unwrap_or_default(),
        _ => Vec::new(),
    };
    list.push(item);
    let _ = state.node.store().set(
        &ns,
        &key,
        &Value(serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())),
    );
    (StatusCode::ACCEPTED, "queued")
}

#[derive(Deserialize)]
struct SendPingRequest {
    to_ticket: String,
    #[serde(default)]
    body_b64: Option<String>,
}

async fn send_ping(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(req): Json<SendPingRequest>,
) -> impl IntoResponse {
    let mut sess = match state
        .node
        .transport()
        .open_session(state.node.profile(), &Ticket::from(req.to_ticket.clone()))
        .await
    {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("open_session error: {e}")),
    };
    let body = req
        .body_b64
        .and_then(|s| base64::prelude::BASE64_STANDARD.decode(s).ok())
        .unwrap_or_default();
    let msg = Message {
        from: state.node.profile().clone(),
        kind: MessageKind::Ping,
        body,
    };
    match sess.send(&msg).await {
        Ok(()) => (StatusCode::ACCEPTED, "sent".to_string()),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("send error: {e}")),
    }
}

pub async fn serve(
    profile: ProfileName,
    addr: SocketAddr,
    public_base_url: String,
) -> anyhow::Result<()> {
    let app = build_app(profile, public_base_url).await;
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
