// #![cfg(target_arch = "wasm32")]

use mee_did_api::DidProvider;
use mee_node_api::{
    IdentityManager as NodeIdentityManager, Invite, InviteManager as NodeInviteManager,
    InviteSignature, Node, UserManager,
};
use mee_sync_api as api;
use mee_types::{NodeId, TransportUserId};
use serde_json as _;
use wasm_bindgen::prelude::*;

// tiny facade just to ensure our core APIs compile to wasm.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[wasm_bindgen]
pub fn did_method_of(did: &str) -> String {
    use mee_types::Did;
    let method = Did(did.to_string()).method();
    method.to_string()
}

#[wasm_bindgen]
pub fn did_key_manager_method() -> String {
    let mgr = mee_did_key::KeyDidManager;
    mgr.method().to_string()
}

#[wasm_bindgen]
pub fn mem_kv_roundtrip_ok(k: &str, v: &str) -> bool {
    use mee_local_store_api::{Key, KvStore, Namespace, Value};
    use mee_local_store_mem::MemKvStore;
    let store = MemKvStore::new();
    let ns = Namespace(k.into());
    let key = Key(k.into());
    let val = Value(v.into());
    if store.set(&ns, &key, &val).is_err() {
        return false;
    }
    match store.get(&ns, &key) {
        Ok(Some(got)) => got.0 == v,
        _ => false,
    }
}

struct WasmNoopSync;

impl WasmNoopSync {
    fn new() -> Self {
        Self
    }
}

struct WasmNode {
    node_id: NodeId,
    did_manager: mee_did_key::KeyDidManager,
    store: mee_local_store_mem::MemKvStore,
    user_manager: WasmUserManager,
    sync_engine: WasmNoopSync,
    invite_manager: WasmInviteManager,
    identity_manager: WasmIdentityManager,
}

impl WasmNode {
    fn new() -> Self {
        Self {
            node_id: NodeId::from("did:key:zwasm"),
            did_manager: mee_did_key::KeyDidManager,
            store: mee_local_store_mem::MemKvStore::new(),
            sync_engine: WasmNoopSync::new(),
            user_manager: WasmUserManager {
                did: mee_types::Did("did:key:zwasm".into()),
            },
            invite_manager: WasmInviteManager::new(),
            identity_manager: WasmIdentityManager::new(),
        }
    }
}

impl mee_node_api::Node for WasmNode {
    type DidManager = mee_did_key::KeyDidManager;
    type KvStore = mee_local_store_mem::MemKvStore;
    type SyncEngine = WasmNoopSync;
    type UserManager = WasmUserManager;
    type InviteManager = WasmInviteManager;
    type IdentityManager = WasmIdentityManager;

    fn node_id(&self) -> &NodeId {
        &self.node_id
    }
    fn did_manager(&self) -> &Self::DidManager {
        &self.did_manager
    }
    fn store(&self) -> &Self::KvStore {
        &self.store
    }
    fn sync_engine(&self) -> &Self::SyncEngine {
        &self.sync_engine
    }
    fn user_manager(&self) -> &Self::UserManager {
        &self.user_manager
    }
    fn invites(&self) -> &Self::InviteManager {
        &self.invite_manager
    }
    fn identities(&self) -> &Self::IdentityManager {
        &self.identity_manager
    }
}

#[wasm_bindgen]
pub fn node_trait_compiles() -> bool {
    let node = WasmNode::new();
    let _ = node.node_id();
    let _ = node.sync_engine();
    let _ = node.user_manager().user_did();
    true
}

#[derive(Clone)]
struct WasmUserManager {
    did: mee_types::Did,
}
impl UserManager for WasmUserManager {
    fn user_did(&self) -> mee_types::Did {
        self.did.clone()
    }
}

#[derive(Clone)]
struct WasmInviteManager {
    ns: api::NamespaceId,
}

impl WasmInviteManager {
    fn new() -> Self {
        Self {
            ns: api::NamespaceId("ns_wasm".into()),
        }
    }
}

#[allow(async_fn_in_trait)]
impl NodeInviteManager for WasmInviteManager {
    fn default_namespace(&self) -> api::NamespaceId {
        self.ns.clone()
    }

    async fn create_invite(&self) -> Result<Invite, api::SyncError> {
        Ok(Invite {
            inviter_did: mee_types::Did("did:key:zwasm".into()),
            transport_user_id: TransportUserId("wasm_user".into()),
            node: api::NodeAddr {
                node_id: NodeId::from("n_wasm"),
                direct_addresses: vec![],
                relay_url: None,
            },
            expires_at: 0,
            sig: InviteSignature::default(),
        })
    }

    async fn ticket_from_invite(
        &self,
        _invite: &Invite,
        _ns: &api::NamespaceId,
        _write: bool,
        _scope: api::ShareScope,
    ) -> Result<api::SyncTicket, api::SyncError> {
        Ok(api::SyncTicket {
            caps: vec![],
            nodes: vec![],
        })
    }

    fn remember(&self, _invite: Invite) {}

    fn invite_for(&self, _did: &mee_types::Did) -> Option<Invite> {
        None
    }
}

#[derive(Clone, Default)]
struct WasmIdentityManager;

impl WasmIdentityManager {
    fn new() -> Self {
        Self
    }
}

#[allow(async_fn_in_trait)]
impl NodeIdentityManager for WasmIdentityManager {
    async fn create_identity(
        &self,
        params: &mee_did_api::DidCreateParams,
    ) -> Result<mee_types::Did, mee_did_api::DidError> {
        let mgr = mee_did_key::KeyDidManager;
        mgr.create(params).await
    }
}

#[wasm_bindgen]
pub fn sync_types_sample_ticket() -> String {
    let ticket = mee_sync_api::SyncTicket {
        caps: vec![serde_json::json!(null)],
        nodes: vec![mee_sync_api::NodeAddr {
            node_id: NodeId::from("n_local_test"),
            direct_addresses: vec![mee_sync_api::DirectAddress::from("127.0.0.1:0")],
            relay_url: None,
        }],
    };
    serde_json::to_string(&ticket).unwrap_or_else(|_| "{}".to_string())
}

#[wasm_bindgen]
pub fn sync_namespace_roundtrip(s: &str) -> String {
    let ns = mee_sync_api::NamespaceId(s.to_string());
    ns.0
}

impl api::SyncEngine for WasmNoopSync {
    async fn addr(&self) -> Result<api::NodeAddr, api::SyncError> {
        Ok(api::NodeAddr {
            node_id: NodeId::from("n_wasm"),
            direct_addresses: vec![],
            relay_url: None,
        })
    }
    async fn user_id(&self) -> Result<TransportUserId, api::SyncError> {
        Ok(TransportUserId("wasm_user".into()))
    }
    async fn create_namespace(
        &self,
        _owner: &TransportUserId,
    ) -> Result<api::NamespaceId, api::SyncError> {
        Ok(api::NamespaceId("ns_wasm".into()))
    }
    async fn list_namespaces(&self) -> Result<Vec<api::NamespaceId>, api::SyncError> {
        Ok(vec![])
    }
    async fn share(
        &self,
        _ns: &api::NamespaceId,
        _to: &TransportUserId,
        _access: api::AccessMode,
        _scope: api::ShareScope,
    ) -> Result<api::SyncTicket, api::SyncError> {
        Ok(api::SyncTicket {
            caps: vec![],
            nodes: vec![],
        })
    }
    async fn import_and_sync(
        &self,
        _ticket: api::SyncTicket,
        _mode: api::SyncMode,
    ) -> Result<Box<dyn api::SyncHandle>, api::SyncError> {
        struct H;
        impl futures_util::Stream for H {
            type Item = api::SyncEvent;
            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                std::task::Poll::Ready(None)
            }
        }
        impl api::SyncHandle for H {
            fn complete<'a>(
                &'a mut self,
            ) -> std::pin::Pin<
                Box<dyn futures_util::Future<Output = Result<(), api::SyncError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
        }
        Ok(Box::new(H))
    }
    async fn insert(
        &self,
        _ns: &api::NamespaceId,
        _path: &api::EntryPath,
        _bytes: &[u8],
    ) -> Result<(), api::SyncError> {
        Ok(())
    }

    type EntryStream = futures_util::stream::Empty<Result<api::EntryInfo, api::SyncError>>;
    async fn get_entries(
        &self,
        _ns: &api::NamespaceId,
    ) -> Result<Self::EntryStream, api::SyncError> {
        Ok(futures_util::stream::empty())
    }
}
