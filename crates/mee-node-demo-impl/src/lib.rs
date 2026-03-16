use std::sync::Arc;

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
use mee_types::local_store::keys;
use mee_types::{Aid, LocalStore, NodeId};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct DemoNode {
    node_id: NodeId,
    store: LocalStore,
    identity: DemoIdentityService,
    trust: DemoTrustService,
    data: DemoDataService,
    sync: DemoSyncService,
}

#[allow(clippy::expect_used)]
impl DemoNode {
    pub async fn spawn(discovery: DiscoveryConfig) -> anyhow::Result<Arc<Self>> {
        let store = LocalStore::new();
        let sync_engine = Arc::new(IrohWillowSyncCore::spawn(discovery, store.clone()).await?);
        let identity_mgr = Arc::new(KeriIdentityManager::new());
        let initial_aid = identity_mgr
            .create()
            .await
            .map_err(|e| anyhow::anyhow!("identity create error: {e}"))?;

        store
            .set_json(keys::CURRENT_AID, &initial_aid)
            .map_err(|e| anyhow::anyhow!("store error: {e}"))?;

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

        let identity = DemoIdentityService {
            identity_mgr: identity_mgr.clone(),
            store: store.clone(),
        };

        let trust = DemoTrustService {
            sync: sync_engine.clone(),
            namespace,
            store: store.clone(),
        };

        let data = DemoDataService {
            sync: sync_engine.clone(),
            namespace,
            store: store.clone(),
        };

        let sync = DemoSyncService {
            sync: sync_engine,
            namespace,
        };

        let node = Self {
            node_id: node_addr.node_id,
            store,
            identity,
            trust,
            data,
            sync,
        };

        Ok(Arc::new(node))
    }

    /// Access the shared local store.
    pub fn store(&self) -> &LocalStore {
        &self.store
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
    store: LocalStore,
}

#[allow(async_fn_in_trait, clippy::expect_used)]
impl IdentityService for DemoIdentityService {
    fn aid(&self) -> Aid {
        self.store
            .get_json::<Aid>(keys::CURRENT_AID)
            .expect("store read")
            .expect("AID not set")
    }

    // TODO: Identity re-creation shouldn't wipe invites/contacts/persona
    // once persistent storage exists. Demo-only shortcut.
    async fn create(&self) -> Result<Aid, mee_identity_api::IdentityError> {
        let aid = self.identity_mgr.create().await?;
        self.store
            .set_json(keys::CURRENT_AID, &aid)
            .expect("store write");
        let _ = self.store.delete_prefix(keys::INVITES_PREFIX);
        let _ = self.store.delete_prefix(keys::CONTACTS_PREFIX);
        let _ = self.store.delete_prefix(keys::PERSONA_PREFIX);
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
    store: LocalStore,
}

#[allow(async_fn_in_trait, clippy::expect_used, clippy::unwrap_in_result)]
impl TrustService for DemoTrustService {
    fn default_namespace(&self) -> NamespaceId {
        self.namespace
    }

    async fn create_invite(&self) -> Result<Invite, SyncError> {
        let inviter: Aid = self
            .store
            .get_json(keys::CURRENT_AID)
            .map_err(|e| SyncError::Backend(e.to_string()))?
            .ok_or_else(|| SyncError::Other("AID not set".into()))?;
        let node_addr = self.sync.addr().await?;
        let subspace_id = self.sync.subspace_id().await?;
        // TODO: Make invite expiry configurable instead of hardcoded 10 minutes.
        let expires_at = now_ms() + 10 * 60 * 1000;
        let mut invite = Invite {
            inviter_aid: inviter,
            namespace_id: self.namespace,
            subspace_id,
            node_hints: vec![node_addr],
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
        let _ = self
            .store
            .set_json(&keys::invite(&invite.inviter_aid), &invite);
    }

    fn invite_for(&self, aid: &Aid) -> Option<Invite> {
        self.store
            .get_json::<Invite>(&keys::invite(aid))
            .expect("store read")
    }

    fn add_contact(&self, contact: Contact) {
        let _ = self.store.set_json(&keys::contact(&contact.aid), &contact);
    }

    fn contact(&self, aid: &Aid) -> Option<Contact> {
        self.store
            .get_json::<Contact>(&keys::contact(aid))
            .expect("store read")
    }

    fn contacts(&self) -> Vec<Contact> {
        self.store
            .values_json::<Contact>(keys::CONTACTS_PREFIX)
            .expect("store read")
    }
}

#[derive(Clone)]
pub struct DemoDataService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
    store: LocalStore,
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
        let _ = self.store.set_json(&keys::persona(key), &value);
        let path = Self::persona_path(key)?;
        self.sync
            .insert(&self.namespace, &path, value.as_bytes())
            .await?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), DataError> {
        let _ = self.store.delete(&keys::persona(key));
        let path = Self::persona_path(key)?;
        self.sync.insert(&self.namespace, &path, &[]).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<DataEntry>, DataError> {
        Ok(self
            .store
            .get_json::<String>(&keys::persona(key))
            .expect("store read")
            .map(|v| DataEntry {
                key: key.to_owned(),
                value: v,
            }))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<DataEntry>, DataError> {
        let full_prefix = keys::persona(prefix);
        let matching_keys = self.store.keys(&full_prefix).expect("store read");
        let mut out = Vec::new();
        for store_key in matching_keys {
            if let Some(persona_key) = store_key.strip_prefix(keys::PERSONA_PREFIX) {
                if let Some(value) = self
                    .store
                    .get_json::<String>(&store_key)
                    .expect("store read")
                {
                    out.push(DataEntry {
                        key: persona_key.to_owned(),
                        value,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone)]
pub struct DemoSyncService {
    sync: Arc<IrohWillowSyncCore>,
    namespace: NamespaceId,
}

impl DemoSyncService {
    /// Access the underlying sync core.
    ///
    /// This is a demo-only escape hatch for debug routes and test
    /// tooling. Not part of the public node API.
    pub fn core(&self) -> &IrohWillowSyncCore {
        &self.sync
    }
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
    let data = format!(
        "demo|{}|{}|{}",
        inv.inviter_aid, inv.namespace_id, inv.subspace_id,
    );
    h.update(data.as_bytes());

    // Hash all node hints deterministically
    let mut all_addrs: Vec<String> = Vec::new();
    for hint in &inv.node_hints {
        all_addrs.push(hint.node_id.to_string());
        for addr in &hint.direct_addresses {
            all_addrs.push(addr.to_string());
        }
        if let Some(ref relay) = hint.relay_url {
            all_addrs.push(relay.to_string());
        }
    }
    all_addrs.sort();
    h.update(all_addrs.join(",").as_bytes());

    h.update(inv.expires_at.to_le_bytes());
    InviteSignature::new(hex::encode(h.finalize()))
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
