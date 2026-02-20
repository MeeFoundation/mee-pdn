use mee_sync_api::{NamespaceId, NodeAddr, SyncError, SyncTicket, TransportUserId};
use mee_types::Did;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub inviter_did: Did,
    pub transport_user_id: TransportUserId,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contact {
    pub did: Did,
    pub alias: Option<String>,
}

#[allow(async_fn_in_trait)]
pub trait TrustService: Send + Sync {
    fn default_namespace(&self) -> NamespaceId;
    async fn create_invite(&self) -> Result<Invite, SyncError>;
    async fn accept_invite(
        &self,
        invite: &Invite,
        write: bool,
    ) -> Result<SyncTicket, SyncError>;
    fn remember_invite(&self, invite: Invite);
    fn invite_for(&self, did: &Did) -> Option<Invite>;
    fn add_contact(&self, contact: Contact);
    fn contact(&self, did: &Did) -> Option<Contact>;
    fn contacts(&self) -> Vec<Contact>;
}
