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
    AccessMode, EntryPath, NamespaceId, SubspaceId, SyncError, SyncHandle, SyncMode, SyncTicket,
};
use mee_sync_iroh_willow::{DiscoveryConfig, IrohWillowSyncCore};
use mee_types::Aid;

#[derive(Clone)]
pub struct DemoNode {
    node_id: mee_types::NodeId,
    namespace: NamespaceId,
    identity: DemoIdentityService,
    trust: DemoTrustService,
    data: DemoDataService,
    sync: DemoSyncService,
}

#[allow(clippy::expect_used)]
impl DemoNode {
    pub async fn spawn(discovery: DiscoveryConfig) -> anyhow::Result<Arc<Self>> {
        let sync_engine = Arc::new(IrohWillowSyncCore::spawn(discovery).await?);
        let identity_mgr = Arc::new(KeriIdentityManager::new());
        let initial_aid = identity_mgr
            .create()
            .await
            .map_err(|e| anyhow::anyhow!("identity create error: {e}"))?;

        let namespace = sync_engine.home_namespace();

        // Write initial AID to Willow _local/ path
        sync_engine
            .put_local_json("identity/current_aid", &initial_aid)
            .await
            .map_err(|e| anyhow::anyhow!("write aid: {e}"))?;

        // Get the actual transport NodeId from the endpoint
        let node_addr = sync_engine
            .addr()
            .await
            .map_err(|e| anyhow::anyhow!("node addr error: {e}"))?;

        let identity = DemoIdentityService {
            identity_mgr: identity_mgr.clone(),
            sync: sync_engine.clone(),
        };

        let trust = DemoTrustService {
            sync: sync_engine.clone(),
            namespace,
        };

        let data = DemoDataService {
            sync: sync_engine.clone(),
        };

        let sync = DemoSyncService {
            sync: sync_engine,
            namespace,
        };

        let node = Self {
            node_id: node_addr.node_id,
            namespace,
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

    fn node_id(&self) -> &mee_types::NodeId {
        &self.node_id
    }

    fn home_namespace(&self) -> NamespaceId {
        self.namespace
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
    sync: Arc<IrohWillowSyncCore>,
}

#[allow(async_fn_in_trait)]
impl IdentityService for DemoIdentityService {
    async fn aid(&self) -> Result<Aid, mee_identity_api::IdentityError> {
        self.sync
            .get_local_json::<Aid>("identity/current_aid")
            .await
            .map_err(|e| mee_identity_api::IdentityError::Other(e.to_string()))?
            .ok_or_else(|| mee_identity_api::IdentityError::Other("AID not set".to_owned()))
    }

    async fn create(&self) -> Result<Aid, mee_identity_api::IdentityError> {
        let aid = self.identity_mgr.create().await?;
        self.sync
            .put_local_json("identity/current_aid", &aid)
            .await
            .map_err(|e| mee_identity_api::IdentityError::Other(e.to_string()))?;
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
}

#[allow(async_fn_in_trait)]
impl TrustService for DemoTrustService {
    async fn create_invite(&self) -> Result<Invite, SyncError> {
        let inviter: Aid = self
            .sync
            .get_local_json("identity/current_aid")
            .await?
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

    async fn remember_invite(&self, invite: Invite) -> Result<(), SyncError> {
        self.sync
            .put_local_json(&format!("invites/{}", invite.inviter_aid), &invite)
            .await
    }

    async fn invite_for(&self, aid: &Aid) -> Result<Option<Invite>, SyncError> {
        self.sync.get_local_json(&format!("invites/{aid}")).await
    }

    async fn add_contact(&self, contact: Contact) -> Result<(), SyncError> {
        self.sync
            .put_local_json(&format!("contacts/{}", contact.aid), &contact)
            .await
    }

    async fn contact(&self, aid: &Aid) -> Result<Option<Contact>, SyncError> {
        self.sync.get_local_json(&format!("contacts/{aid}")).await
    }

    async fn contacts(&self) -> Result<Vec<Contact>, SyncError> {
        let entries = self.sync.list_local("contacts/").await?;
        let mut out = Vec::new();
        for (_, bytes) in entries {
            if !bytes.is_empty() {
                if let Ok(contact) = serde_json::from_slice::<Contact>(&bytes) {
                    out.push(contact);
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone)]
pub struct DemoDataService {
    sync: Arc<IrohWillowSyncCore>,
}

impl DemoDataService {
    fn data_path(key: &str) -> Result<EntryPath, DataError> {
        EntryPath::new(format!("data/{key}"))
            .map_err(|e| DataError::Sync(SyncError::Backend(format!("invalid data path: {e}"))))
    }
}

#[allow(async_fn_in_trait)]
impl mee_node_api::DataService for DemoDataService {
    async fn set(&self, ns: &NamespaceId, key: &str, value: &[u8]) -> Result<(), DataError> {
        let path = Self::data_path(key)?;
        self.sync.insert(ns, &path, value).await?;
        Ok(())
    }

    async fn delete(&self, ns: &NamespaceId, key: &str) -> Result<(), DataError> {
        let path = Self::data_path(key)?;
        self.sync.insert(ns, &path, &[]).await?;
        Ok(())
    }

    async fn get(&self, ns: &NamespaceId, key: &str) -> Result<Option<DataEntry>, DataError> {
        let path = Self::data_path(key)?;
        match self.sync.read_entry_payload(ns, &path).await? {
            Some(bytes) if !bytes.is_empty() => Ok(Some(DataEntry {
                key: key.to_owned(),
                value: bytes,
            })),
            _ => Ok(None),
        }
    }

    async fn list(&self, ns: &NamespaceId, prefix: &str) -> Result<Vec<DataEntry>, DataError> {
        let mut out = Vec::new();
        let mut stream = self.sync.get_entries(ns).await?;
        while let Some(Ok(entry)) = stream.next().await {
            let path_str = entry.path.as_str();
            if let Some(key) = path_str.strip_prefix("data/") {
                if key.starts_with(prefix) {
                    let value = self
                        .sync
                        .read_entry_payload(ns, &entry.path)
                        .await?
                        .unwrap_or_default();
                    out.push(DataEntry {
                        key: key.to_owned(),
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
            .connect(&self.namespace, to, peer_addr, access)
            .await
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
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
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
