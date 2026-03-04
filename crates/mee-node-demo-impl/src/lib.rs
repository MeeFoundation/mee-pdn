use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;

use mee_did_api::{DidCreateParams, DidKeyCreateOptions, DidProvider, DidResolver};
use mee_did_key::KeyDidManager;
use mee_local_store_mem::MemKvStore;
use mee_node_api::{
    Contact, DataEntry, IdentityService, Invite, InviteSignature, Node, SyncService, TrustService,
};
use mee_sync_api as api;
use mee_sync_api::SyncEngine;
use mee_sync_api::{
    AccessMode, EntryInfo, EntryPath, NamespaceId, SyncError, SyncHandle, SyncMode, SyncTicket,
    TransportUserId,
};
use mee_sync_iroh_willow::{DiscoveryConfig, IrohWillowSyncCore};
use mee_types::{Did, NodeId};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct DemoNode {
    node_id: NodeId,
    store: MemKvStore,
    identity: DemoIdentityService,
    trust: DemoTrustService,
    data: DemoDataService,
    sync: DemoSyncService,
}

#[allow(clippy::expect_used)]
impl DemoNode {
    pub async fn spawn(discovery: DiscoveryConfig) -> anyhow::Result<Arc<Self>> {
        let sync_engine = Arc::new(IrohWillowSyncCore::spawn(discovery).await?);
        let did_mgr = Arc::new(KeyDidManager);
        let params = DidCreateParams::Key(DidKeyCreateOptions {
            jwk: String::new(),
            use_jcs_pub: true,
        });
        let initial_did = did_mgr
            .create(&params)
            .await
            .map_err(|e| anyhow::anyhow!("did create error: {e}"))?;
        let current_did = Arc::new(Mutex::new(initial_did));

        let owner = sync_engine
            .user_id()
            .await
            .map_err(|e| anyhow::anyhow!("willow user id error: {e}"))?;
        let namespace = sync_engine
            .create_namespace(&owner)
            .await
            .map_err(|e| anyhow::anyhow!("namespace create error: {e}"))?;

        let invites = Arc::new(Mutex::new(HashMap::<Did, Invite>::new()));
        let contacts = Arc::new(Mutex::new(HashMap::<Did, Contact>::new()));
        let persona = Arc::new(Mutex::new(HashMap::<String, String>::new()));

        let identity = DemoIdentityService {
            did_mgr: did_mgr.clone(),
            current: current_did.clone(),
            invites: invites.clone(),
            contacts: contacts.clone(),
            persona: persona.clone(),
        };

        let trust = DemoTrustService {
            sync: sync_engine.clone(),
            namespace: namespace.clone(),
            current_did: current_did.clone(),
            invites,
            contacts,
        };

        let data = DemoDataService {
            sync: sync_engine.clone(),
            namespace: namespace.clone(),
            persona,
        };

        let sync = DemoSyncService {
            sync: sync_engine,
            namespace,
        };

        let node = Self {
            node_id: NodeId::from(
                current_did
                    .lock()
                    .expect("current DID lock poisoned")
                    .as_ref(),
            ),
            store: MemKvStore::new(),
            identity,
            trust,
            data,
            sync,
        };

        Ok(Arc::new(node))
    }
}

impl Node for DemoNode {
    type Store = MemKvStore;
    type Identity = DemoIdentityService;
    type Trust = DemoTrustService;
    type Data = DemoDataService;
    type Sync = DemoSyncService;

    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn store(&self) -> &Self::Store {
        &self.store
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

#[derive(Clone)]
pub struct DemoIdentityService {
    did_mgr: Arc<KeyDidManager>,
    current: Arc<Mutex<Did>>,
    invites: Arc<Mutex<HashMap<Did, Invite>>>,
    contacts: Arc<Mutex<HashMap<Did, Contact>>>,
    persona: Arc<Mutex<HashMap<String, String>>>,
}

#[allow(async_fn_in_trait, clippy::expect_used)]
impl IdentityService for DemoIdentityService {
    fn current(&self) -> Did {
        self.current
            .lock()
            .expect("current DID lock poisoned")
            .clone()
    }

    async fn create(&self, params: &DidCreateParams) -> Result<Did, mee_did_api::DidError> {
        let did = self.did_mgr.create(params).await?;
        *self.current.lock().expect("current DID lock poisoned") = did.clone();
        self.invites.lock().expect("invites lock poisoned").clear();
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .clear();
        self.persona.lock().expect("persona lock poisoned").clear();
        Ok(did)
    }

    async fn resolve(&self, did: &Did) -> Result<mee_did_api::DidDocument, mee_did_api::DidError> {
        self.did_mgr.resolve(did).await
    }
}

#[derive(Clone)]
pub struct DemoTrustService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
    current_did: Arc<Mutex<Did>>,
    invites: Arc<Mutex<HashMap<Did, Invite>>>,
    contacts: Arc<Mutex<HashMap<Did, Contact>>>,
}

#[allow(async_fn_in_trait, clippy::expect_used, clippy::unwrap_in_result)]
impl TrustService for DemoTrustService {
    fn default_namespace(&self) -> NamespaceId {
        self.namespace.clone()
    }

    async fn create_invite(&self) -> Result<Invite, SyncError> {
        let inviter = self
            .current_did
            .lock()
            .expect("current DID lock poisoned")
            .clone();
        let node = self.sync.addr().await?;
        let transport_user_id = self.sync.user_id().await?;
        let expires_at = now_ms() + 10 * 60 * 1000;
        let mut invite = Invite {
            inviter_did: inviter,
            transport_user_id,
            node,
            expires_at,
            sig: InviteSignature::default(),
        };
        invite.sig = compute_invite_sig(&invite);
        Ok(invite)
    }

    async fn accept_invite(&self, invite: &Invite, write: bool) -> Result<SyncTicket, SyncError> {
        verify_invite(invite)?;
        let mode = if write {
            AccessMode::Write
        } else {
            AccessMode::Read
        };
        self.sync
            .share(&self.namespace, &invite.transport_user_id, mode)
            .await
    }

    fn remember_invite(&self, invite: Invite) {
        self.invites
            .lock()
            .expect("invites lock poisoned")
            .insert(invite.inviter_did.clone(), invite);
    }

    fn invite_for(&self, did: &Did) -> Option<Invite> {
        self.invites
            .lock()
            .expect("invites lock poisoned")
            .get(did)
            .cloned()
    }

    fn add_contact(&self, contact: Contact) {
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .insert(contact.did.clone(), contact);
    }

    fn contact(&self, did: &Did) -> Option<Contact> {
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .get(did)
            .cloned()
    }

    fn contacts(&self) -> Vec<Contact> {
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .values()
            .cloned()
            .collect()
    }
}

#[derive(Clone)]
pub struct DemoDataService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
    persona: Arc<Mutex<HashMap<String, String>>>,
}

impl DemoDataService {
    fn persona_path(key: &str) -> EntryPath {
        EntryPath(format!("persona/{key}"))
    }
}

#[allow(async_fn_in_trait, clippy::expect_used)]
impl mee_node_api::DataService for DemoDataService {
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.persona
            .lock()
            .expect("persona lock poisoned")
            .insert(key.to_owned(), value.to_owned());
        let path = Self::persona_path(key);
        self.sync
            .insert(&self.namespace, &path, value.as_bytes())
            .await
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.persona
            .lock()
            .expect("persona lock poisoned")
            .remove(key);
        let path = Self::persona_path(key);
        self.sync
            .insert(&self.namespace, &path, &[])
            .await
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<DataEntry>> {
        Ok(self
            .persona
            .lock()
            .expect("persona lock poisoned")
            .get(key)
            .map(|v| DataEntry {
                key: key.to_owned(),
                value: v.clone(),
            }))
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<DataEntry>> {
        Ok(self
            .persona
            .lock()
            .expect("persona lock poisoned")
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| DataEntry {
                key: k.clone(),
                value: v.clone(),
            })
            .collect())
    }
}

#[derive(Clone)]
pub struct DemoSyncService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
}

#[allow(async_fn_in_trait)]
impl SyncService for DemoSyncService {
    async fn node_addr(&self) -> Result<api::NodeAddr, SyncError> {
        self.sync.addr().await
    }

    async fn user_id(&self) -> Result<TransportUserId, SyncError> {
        self.sync.user_id().await
    }

    async fn share(&self, to: &TransportUserId, write: bool) -> Result<SyncTicket, SyncError> {
        let mode = if write {
            AccessMode::Write
        } else {
            AccessMode::Read
        };
        self.sync.share(&self.namespace, to, mode).await
    }

    async fn import(
        &self,
        ticket: SyncTicket,
        mode: SyncMode,
    ) -> Result<Box<dyn SyncHandle>, SyncError> {
        self.sync.import_and_sync(ticket, mode).await
    }

    async fn connect_to_peer(
        &self,
        to: &TransportUserId,
        peer_addr: &api::NodeAddr,
        write: bool,
    ) -> Result<(), SyncError> {
        let access = if write {
            AccessMode::Write
        } else {
            AccessMode::Read
        };
        self.sync
            .connect_and_share(&self.namespace, to, peer_addr, access)
            .await
    }

    async fn insert(&self, path: &EntryPath, bytes: &[u8]) -> Result<(), SyncError> {
        self.sync.insert(&self.namespace, path, bytes).await
    }

    async fn list(&self) -> Result<Vec<EntryInfo>, SyncError> {
        let mut all = Vec::new();
        let namespaces = self.sync.list_namespaces().await?;
        for ns in &namespaces {
            let mut stream = self.sync.get_entries(ns).await?;
            while let Some(item) = stream.next().await {
                let entry = item?;
                all.push(entry);
            }
        }
        Ok(all)
    }
}

#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Truncation is safe: u64 millis covers ~584 million years from epoch.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

fn compute_invite_sig(inv: &Invite) -> InviteSignature {
    let mut h = Sha256::new();
    let relay = inv
        .node
        .relay_url
        .as_ref()
        .map(std::string::ToString::to_string)
        .unwrap_or_default();
    let mut addrs: Vec<String> = inv
        .node
        .direct_addresses
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    addrs.sort();
    let data = format!(
        "demo|{}|{}|{}|{}|{}",
        inv.inviter_did,
        inv.transport_user_id,
        inv.node.node_id.as_ref(),
        addrs.join(","),
        relay,
    );
    h.update(data.as_bytes());
    h.update(inv.expires_at.to_le_bytes());
    let out = h.finalize();
    InviteSignature::new(hex::encode(out))
}

fn verify_invite(inv: &Invite) -> Result<(), SyncError> {
    if now_ms() > inv.expires_at {
        return Err(SyncError::Other("invite expired".into()));
    }
    let expect = compute_invite_sig(inv);
    if expect != inv.sig {
        return Err(SyncError::Other("invalid invite signature".into()));
    }
    Ok(())
}
