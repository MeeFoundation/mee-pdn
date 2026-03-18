use mee_sync_api::{AccessMode, NamespaceId, NodeAddr, SubspaceId, SyncError, SyncTicket};
use mee_types::Aid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub inviter_aid: Aid,
    /// Which namespace this invite grants access to.
    pub namespace_id: NamespaceId,
    pub subspace_id: SubspaceId,
    /// Connection hints — addresses where the inviter was at creation
    /// time. May be stale or empty; gossip provides fallback.
    pub node_hints: Vec<NodeAddr>,
    pub expires_at: u64,
    pub sig: InviteSignature,
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InviteSignature(pub String);

impl InviteSignature {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// TODO(personal-namespaces): Add primary_namespace field to link contacts to the
// peer's home namespace for capability delegation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contact {
    pub aid: Aid,
    pub alias: Option<String>,
}

#[allow(async_fn_in_trait)]
pub trait TrustService: Send + Sync {
    async fn create_invite(&self) -> Result<Invite, SyncError>;
    async fn accept_invite(
        &self,
        invite: &Invite,
        access: AccessMode,
    ) -> Result<SyncTicket, SyncError>;
    async fn remember_invite(&self, invite: Invite) -> Result<(), SyncError>;
    async fn invite_for(&self, aid: &Aid) -> Result<Option<Invite>, SyncError>;
    async fn add_contact(&self, contact: Contact) -> Result<(), SyncError>;
    async fn contact(&self, aid: &Aid) -> Result<Option<Contact>, SyncError>;
    async fn contacts(&self) -> Result<Vec<Contact>, SyncError>;
}
