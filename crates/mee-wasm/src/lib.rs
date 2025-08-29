#![cfg(target_arch = "wasm32")]

use mee_did_api::DidProvider;
use mee_node_api::Node;
use mee_types::{NodeId, UserId};
use wasm_bindgen::prelude::*;

// tiny facade just to ensure our core APIs compile to wasm.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[wasm_bindgen]
pub fn parse_message_kind(s: &str) -> String {
    let mk = mee_transport_api::MessageKind::from(s);
    mk.to_string()
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

// no-op mee-node-api impl
struct WasmNoopTransport;
struct WasmNoopSession;

impl mee_transport_api::Session for WasmNoopSession {
    async fn send(&mut self, _msg: &mee_transport_api::Message) -> std::io::Result<()> {
        Ok(())
    }
    async fn recv(&mut self) -> std::io::Result<Option<mee_transport_api::Message>> {
        Ok(None)
    }
}

impl mee_transport_api::Transport for WasmNoopTransport {
    type Sess = WasmNoopSession;
    async fn ticket(
        &self,
        _profile: &mee_transport_api::ProfileName,
    ) -> std::io::Result<mee_transport_api::Ticket> {
        Ok(mee_transport_api::Ticket("noop".to_string()))
    }
    async fn open_session(
        &self,
        _local: &mee_transport_api::ProfileName,
        _remote: &mee_transport_api::Ticket,
    ) -> std::io::Result<Self::Sess> {
        Ok(WasmNoopSession)
    }
}

struct WasmNode {
    profile: mee_transport_api::ProfileName,
    node_id: NodeId,
    transport: WasmNoopTransport,
    provider: mee_did_key::KeyDidManager,
    store: mee_local_store_mem::MemKvStore,
}

impl WasmNode {
    fn new() -> Self {
        Self {
            profile: mee_transport_api::ProfileName::from("wasm"),
            node_id: NodeId::from("did:key:zwasm"),
            transport: WasmNoopTransport,
            provider: mee_did_key::KeyDidManager,
            store: mee_local_store_mem::MemKvStore::new(),
        }
    }
}

impl mee_node_api::Node for WasmNode {
    type Transport = WasmNoopTransport;
    type DidManager = mee_did_key::KeyDidManager;
    type Store = mee_local_store_mem::MemKvStore;

    fn profile(&self) -> &mee_transport_api::ProfileName {
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
        &self.provider
    }
    fn store(&self) -> &Self::Store {
        &self.store
    }
}

#[wasm_bindgen]
pub fn node_trait_compiles() -> bool {
    let node = WasmNode::new();
    let _ = node.profile();
    let _ = node.node_id();
    true
}
