use mee_identity_api::{IdentityError, IdentityState};
use mee_types::Aid;

#[allow(async_fn_in_trait)]
pub trait IdentityService: Send + Sync {
    /// The node's AID. Set once at inception, never changes.
    fn aid(&self) -> Aid;

    /// Create a new identity (KERI inception).
    // TODO(keri): Should initialize local KEL with inception event.
    async fn create(&self) -> Result<Aid, IdentityError>;

    /// Resolve a peer's identity state.
    // TODO(keri): Should verify peer's KEL in direct mode.
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError>;
}
