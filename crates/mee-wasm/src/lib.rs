// #![cfg(target_arch = "wasm32")]

use mee_did_api::DidProvider;
use mee_node_api::{
    Contact, DataEntry, DataError, IdentityService, Invite, InviteSignature, Node, SyncService,
    TrustService,
};
use mee_sync_api as api;
use mee_sync_api::AccessMode;
use mee_types::{Did, NodeId};
use serde_json as _;
use wasm_bindgen::prelude::*;

/// Placeholder zero ID for WASM stubs.
const ZERO_ID: [u8; 32] = [0u8; 32];

// tiny facade just to ensure our core APIs compile to wasm.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[wasm_bindgen]
pub fn did_method_of(did: &str) -> String {
    let method = Did(did.to_owned()).method();
    method.to_string()
}

#[wasm_bindgen]
pub fn did_key_manager_method() -> String {
    let mgr = mee_did_key::KeyDidManager;
    mgr.method().to_string()
}

// --- Noop implementations for WASM build-check ---

#[allow(dead_code)]
struct WasmNoopSync;

struct WasmNode {
    node_id: NodeId,
    identity: WasmIdentityService,
    trust: WasmTrustService,
    data: WasmDataService,
    sync: WasmSyncService,
}

impl WasmNode {
    fn new() -> Self {
        Self {
            node_id: NodeId::from_bytes(ZERO_ID),
            identity: WasmIdentityService,
            trust: WasmTrustService,
            data: WasmDataService,
            sync: WasmSyncService,
        }
    }
}

impl Node for WasmNode {
    type Identity = WasmIdentityService;
    type Trust = WasmTrustService;
    type Data = WasmDataService;
    type Sync = WasmSyncService;

    fn node_id(&self) -> &NodeId {
        &self.node_id
    }
    fn identity(&self) -> &Self::Identity {
        &self.identity
    }
    fn trust(&self) -> &Self::Trust {
        &self.trust
    }
    fn data(&self) -> &Self::Data {
        &self.data
    }
    fn sync(&self) -> &Self::Sync {
        &self.sync
    }
}

#[wasm_bindgen]
pub fn node_trait_compiles() -> bool {
    let node = WasmNode::new();
    let _ = node.node_id();
    true
}

// --- IdentityService ---

#[derive(Clone)]
pub struct WasmIdentityService;

#[allow(async_fn_in_trait)]
impl IdentityService for WasmIdentityService {
    fn current(&self) -> Did {
        Did("did:key:zwasm".into())
    }
    async fn create(
        &self,
        params: &mee_did_api::DidCreateParams,
    ) -> Result<Did, mee_did_api::DidError> {
        let mgr = mee_did_key::KeyDidManager;
        mgr.create(params).await
    }
    async fn resolve(&self, _did: &Did) -> Result<mee_did_api::DidDocument, mee_did_api::DidError> {
        Ok(mee_did_api::DidDocument {
            id: Did("did:key:zwasm".into()),
            verification_method_ids: vec![],
        })
    }
}

// --- TrustService ---

#[derive(Clone)]
pub struct WasmTrustService;

#[allow(async_fn_in_trait)]
impl TrustService for WasmTrustService {
    fn default_namespace(&self) -> api::NamespaceId {
        api::NamespaceId::from_bytes(ZERO_ID)
    }
    async fn create_invite(&self) -> Result<Invite, api::SyncError> {
        Ok(Invite {
            inviter_did: Did("did:key:zwasm".into()),
            subspace_id: api::SubspaceId::from_bytes(ZERO_ID),
            node: api::NodeAddr {
                node_id: NodeId::from_bytes(ZERO_ID),
                direct_addresses: vec![],
                relay_url: None,
            },
            expires_at: 0,
            sig: InviteSignature::default(),
        })
    }
    async fn accept_invite(
        &self,
        _invite: &Invite,
        _access: AccessMode,
    ) -> Result<api::SyncTicket, api::SyncError> {
        Ok(api::SyncTicket {
            caps: vec![],
            nodes: vec![],
        })
    }
    fn remember_invite(&self, _invite: Invite) {}
    fn invite_for(&self, _did: &Did) -> Option<Invite> {
        None
    }
    fn add_contact(&self, _contact: Contact) {}
    fn contact(&self, _did: &Did) -> Option<Contact> {
        None
    }
    fn contacts(&self) -> Vec<Contact> {
        vec![]
    }
}

// --- DataService ---

#[derive(Clone)]
pub struct WasmDataService;

#[allow(async_fn_in_trait)]
impl mee_node_api::DataService for WasmDataService {
    async fn set(&self, _key: &str, _value: &str) -> Result<(), DataError> {
        Ok(())
    }
    async fn delete(&self, _key: &str) -> Result<(), DataError> {
        Ok(())
    }
    async fn get(&self, _key: &str) -> Result<Option<DataEntry>, DataError> {
        Ok(None)
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<DataEntry>, DataError> {
        Ok(vec![])
    }
}

// --- SyncService ---

#[derive(Clone)]
pub struct WasmSyncService;

#[allow(async_fn_in_trait)]
impl SyncService for WasmSyncService {
    async fn node_addr(&self) -> Result<api::NodeAddr, api::SyncError> {
        Ok(api::NodeAddr {
            node_id: NodeId::from_bytes(ZERO_ID),
            direct_addresses: vec![],
            relay_url: None,
        })
    }
    async fn subspace_id(&self) -> Result<api::SubspaceId, api::SyncError> {
        Ok(api::SubspaceId::from_bytes(ZERO_ID))
    }
    async fn share(
        &self,
        _to: &api::SubspaceId,
        _access: AccessMode,
    ) -> Result<api::SyncTicket, api::SyncError> {
        Ok(api::SyncTicket {
            caps: vec![],
            nodes: vec![],
        })
    }
    async fn import(
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
    async fn connect_to_peer(
        &self,
        _to: &api::SubspaceId,
        _peer_addr: &api::NodeAddr,
        _access: AccessMode,
    ) -> Result<(), api::SyncError> {
        Ok(())
    }
    async fn insert(&self, _path: &api::EntryPath, _bytes: &[u8]) -> Result<(), api::SyncError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<api::EntryInfo>, api::SyncError> {
        Ok(vec![])
    }
}

// --- SyncEngine (noop for WASM) ---

impl api::SyncEngine for WasmNoopSync {
    async fn addr(&self) -> Result<api::NodeAddr, api::SyncError> {
        Ok(api::NodeAddr {
            node_id: NodeId::from_bytes(ZERO_ID),
            direct_addresses: vec![],
            relay_url: None,
        })
    }
    async fn subspace_id(&self) -> Result<api::SubspaceId, api::SyncError> {
        Ok(api::SubspaceId::from_bytes(ZERO_ID))
    }
    async fn create_namespace(
        &self,
        _owner: &api::SubspaceId,
    ) -> Result<api::NamespaceId, api::SyncError> {
        Ok(api::NamespaceId::from_bytes(ZERO_ID))
    }
    async fn list_namespaces(&self) -> Result<Vec<api::NamespaceId>, api::SyncError> {
        Ok(vec![])
    }
    async fn share(
        &self,
        _ns: &api::NamespaceId,
        _to: &api::SubspaceId,
        _access: api::AccessMode,
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
    async fn connect_and_share(
        &self,
        _ns: &api::NamespaceId,
        _to: &api::SubspaceId,
        _peer_addr: &api::NodeAddr,
        _access: api::AccessMode,
    ) -> Result<(), api::SyncError> {
        Ok(())
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

#[wasm_bindgen]
#[allow(clippy::expect_used)]
pub fn sync_types_sample_ticket() -> String {
    let ticket = api::SyncTicket {
        caps: vec![serde_json::json!(null)],
        nodes: vec![api::NodeAddr {
            node_id: NodeId::from_bytes(ZERO_ID),
            direct_addresses: vec![api::DirectAddress::from(
                "127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .expect("hardcoded addr"),
            )],
            relay_url: None,
        }],
    };
    serde_json::to_string(&ticket).unwrap_or_else(|_| "{}".to_owned())
}

#[wasm_bindgen]
pub fn sync_namespace_roundtrip(s: &str) -> String {
    // Parse hex string → NamespaceId → back to hex
    let ns: api::NamespaceId = s
        .parse()
        .unwrap_or_else(|_| api::NamespaceId::from_bytes(ZERO_ID));
    ns.to_string()
}
