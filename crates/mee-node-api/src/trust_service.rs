use mee_sync_api::{AccessMode, NamespaceId, NodeAddr, SubspaceId, SyncError, SyncTicket};
use mee_types::Aid;
use serde::{Deserialize, Serialize};

// TODO(personal-namespaces): Add namespace_id field to track which peer namespace
// this invite grants access to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub inviter_aid: Aid,
    pub subspace_id: SubspaceId,
    pub node: NodeAddr,
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
    // TODO(personal-namespaces): Replace with home_namespace() that returns the
    // node's personal namespace. Update share flow to delegate scoped
    // capabilities on both peers' home namespaces.
    fn default_namespace(&self) -> NamespaceId;
    async fn create_invite(&self) -> Result<Invite, SyncError>;
    async fn accept_invite(
        &self,
        invite: &Invite,
        access: AccessMode,
    ) -> Result<SyncTicket, SyncError>;
    fn remember_invite(&self, invite: Invite);
    fn invite_for(&self, aid: &Aid) -> Option<Invite>;
    fn add_contact(&self, contact: Contact);
    fn contact(&self, aid: &Aid) -> Option<Contact>;
    fn contacts(&self) -> Vec<Contact>;
}
