use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;

use mee_identity_api::{IdentityProvider, IdentityResolver};
use mee_identity_keri::KeriIdentityManager;
use mee_node_api::{
    Contact, DataEntry, DataError, IdentityService, Invite, InviteSignature, Node, SyncService,
    TrustService,
};
use mee_sync_api as api;
use mee_sync_api::SyncEngine;
use mee_sync_api::{
    AccessMode, EntryInfo, EntryPath, NamespaceId, SubspaceId, SyncError, SyncHandle, SyncMode,
    SyncTicket,
};
use mee_sync_iroh_willow::{DiscoveryConfig, IrohWillowSyncCore};
use mee_types::{Aid, NodeId};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct DemoNode {
    node_id: NodeId,
    identity: DemoIdentityService,
    trust: DemoTrustService,
    data: DemoDataService,
    sync: DemoSyncService,
}

#[allow(clippy::expect_used)]
impl DemoNode {
    // TODO(persistent-storage): Accept data_dir: Option<PathBuf> and pass through
    // to IrohWillowSyncCore::spawn().
    pub async fn spawn(discovery: DiscoveryConfig) -> anyhow::Result<Arc<Self>> {
        let sync_engine = Arc::new(IrohWillowSyncCore::spawn(discovery).await?);
        let identity_mgr = Arc::new(KeriIdentityManager::new());
        let initial_aid = identity_mgr
            .create()
            .await
            .map_err(|e| anyhow::anyhow!("identity create error: {e}"))?;

        let owner = sync_engine
            .subspace_id()
            .await
            .map_err(|e| anyhow::anyhow!("willow subspace id error: {e}"))?;
        // TODO(personal-namespaces): Replace create_namespace() with home_namespace().
        // The node should use a single personal namespace, not ad-hoc ones.
        let namespace = sync_engine
            .create_namespace(&owner)
            .await
            .map_err(|e| anyhow::anyhow!("namespace create error: {e}"))?;

        // Get the actual transport NodeId from the endpoint
        let node_addr = sync_engine
            .addr()
            .await
            .map_err(|e| anyhow::anyhow!("node addr error: {e}"))?;

        // TODO(persistent-storage): Replace in-memory HashMaps with persistent storage
        // (redb or _local/ entries in home namespace). Lost on restart.
        let invites = Arc::new(Mutex::new(HashMap::<Aid, Invite>::new()));
        let contacts = Arc::new(Mutex::new(HashMap::<Aid, Contact>::new()));
        let persona = Arc::new(Mutex::new(HashMap::<String, String>::new()));

        let current_aid = Arc::new(Mutex::new(initial_aid));

        let identity = DemoIdentityService {
            identity_mgr: identity_mgr.clone(),
            current: current_aid.clone(),
            invites: invites.clone(),
            contacts: contacts.clone(),
            persona: persona.clone(),
        };

        let trust = DemoTrustService {
            sync: sync_engine.clone(),
            namespace,
            current_aid,
            invites,
            contacts,
        };

        let data = DemoDataService {
            sync: sync_engine.clone(),
            namespace,
            persona,
        };

        let sync = DemoSyncService {
            sync: sync_engine,
            namespace,
        };

        let node = Self {
            node_id: node_addr.node_id,
            identity,
            trust,
            data,
            sync,
        };

        Ok(Arc::new(node))
    }
}

impl Node for DemoNode {
    type Identity = DemoIdentityService;
    type Trust = DemoTrustService;
    type Data = DemoDataService;
    type Sync = DemoSyncService;

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

#[derive(Clone)]
pub struct DemoIdentityService {
    identity_mgr: Arc<KeriIdentityManager>,
    current: Arc<Mutex<Aid>>,
    invites: Arc<Mutex<HashMap<Aid, Invite>>>,
    contacts: Arc<Mutex<HashMap<Aid, Contact>>>,
    persona: Arc<Mutex<HashMap<String, String>>>,
}

#[allow(async_fn_in_trait, clippy::expect_used)]
impl IdentityService for DemoIdentityService {
    fn aid(&self) -> Aid {
        *self.current.lock().expect("current AID lock poisoned")
    }

    // TODO: Identity re-creation shouldn't wipe invites/contacts/persona
    // once persistent storage exists. Demo-only shortcut.
    async fn create(&self) -> Result<Aid, mee_identity_api::IdentityError> {
        let aid = self.identity_mgr.create().await?;
        *self.current.lock().expect("current AID lock poisoned") = aid;
        self.invites.lock().expect("invites lock poisoned").clear();
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .clear();
        self.persona.lock().expect("persona lock poisoned").clear();
        Ok(aid)
    }

    async fn resolve(
        &self,
        aid: &Aid,
    ) -> Result<mee_identity_api::IdentityState, mee_identity_api::IdentityError> {
        self.identity_mgr.resolve(aid).await
    }
}

#[derive(Clone)]
pub struct DemoTrustService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
    current_aid: Arc<Mutex<Aid>>,
    invites: Arc<Mutex<HashMap<Aid, Invite>>>,
    contacts: Arc<Mutex<HashMap<Aid, Contact>>>,
}

#[allow(async_fn_in_trait, clippy::expect_used, clippy::unwrap_in_result)]
impl TrustService for DemoTrustService {
    fn default_namespace(&self) -> NamespaceId {
        self.namespace
    }

    async fn create_invite(&self) -> Result<Invite, SyncError> {
        let inviter = *self.current_aid.lock().expect("current AID lock poisoned");
        let node = self.sync.addr().await?;
        let subspace_id = self.sync.subspace_id().await?;
        // TODO: Make invite expiry configurable instead of hardcoded 10 minutes.
        let expires_at = now_ms() + 10 * 60 * 1000;
        let mut invite = Invite {
            inviter_aid: inviter,
            subspace_id,
            node,
            expires_at,
            sig: InviteSignature::default(),
        };
        invite.sig = compute_invite_sig(&invite);
        Ok(invite)
    }

    async fn accept_invite(
        &self,
        invite: &Invite,
        access: AccessMode,
    ) -> Result<SyncTicket, SyncError> {
        verify_invite(invite)?;
        self.sync
            .share(&self.namespace, &invite.subspace_id, access)
            .await
    }

    fn remember_invite(&self, invite: Invite) {
        self.invites
            .lock()
            .expect("invites lock poisoned")
            .insert(invite.inviter_aid, invite);
    }

    fn invite_for(&self, aid: &Aid) -> Option<Invite> {
        self.invites
            .lock()
            .expect("invites lock poisoned")
            .get(aid)
            .cloned()
    }

    fn add_contact(&self, contact: Contact) {
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .insert(contact.aid, contact);
    }

    fn contact(&self, aid: &Aid) -> Option<Contact> {
        self.contacts
            .lock()
            .expect("contacts lock poisoned")
            .get(aid)
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
    fn persona_path(key: &str) -> Result<EntryPath, DataError> {
        EntryPath::new(format!("persona/{key}"))
            .map_err(|e| DataError::Sync(SyncError::Backend(format!("invalid persona path: {e}"))))
    }
}

#[allow(async_fn_in_trait, clippy::expect_used)]
impl mee_node_api::DataService for DemoDataService {
    async fn set(&self, key: &str, value: &str) -> Result<(), DataError> {
        self.persona
            .lock()
            .expect("persona lock poisoned")
            .insert(key.to_owned(), value.to_owned());
        let path = Self::persona_path(key)?;
        self.sync
            .insert(&self.namespace, &path, value.as_bytes())
            .await?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), DataError> {
        self.persona
            .lock()
            .expect("persona lock poisoned")
            .remove(key);
        let path = Self::persona_path(key)?;
        self.sync.insert(&self.namespace, &path, &[]).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<DataEntry>, DataError> {
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

    async fn list(&self, prefix: &str) -> Result<Vec<DataEntry>, DataError> {
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

    async fn subspace_id(&self) -> Result<SubspaceId, SyncError> {
        self.sync.subspace_id().await
    }

    async fn share(&self, to: &SubspaceId, access: AccessMode) -> Result<SyncTicket, SyncError> {
        self.sync.share(&self.namespace, to, access).await
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
        to: &SubspaceId,
        peer_addr: &api::NodeAddr,
        access: AccessMode,
    ) -> Result<(), SyncError> {
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
    // Truncation is safe: u64 millis covers ~584M years.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

// TODO(keri): Replace SHA256 placeholder with Ed25519 signature using
// the operational private key from the signer's KEL.
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
        inv.inviter_aid,
        inv.subspace_id,
        inv.node.node_id,
        addrs.join(","),
        relay,
    );
    h.update(data.as_bytes());
    h.update(inv.expires_at.to_le_bytes());
    let out = h.finalize();
    InviteSignature::new(hex::encode(out))
}

// TODO(keri): Replace with Ed25519 signature verification using the
// resolved IdentityState's operational public key.
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
