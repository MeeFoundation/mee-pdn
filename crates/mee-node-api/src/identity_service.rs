use mee_did_api::{DidCreateParams, DidDocument, DidError};
use mee_types::Did;

#[allow(async_fn_in_trait)]
pub trait IdentityService: Send + Sync {
    fn current(&self) -> Did;
    async fn create(&self, params: &DidCreateParams) -> Result<Did, DidError>;
    async fn resolve(&self, did: &Did) -> Result<DidDocument, DidError>;
}
