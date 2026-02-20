mod error;
pub use error::DidError;
use mee_types::{Did, DidMethod, DidUrl};

#[derive(Clone, Debug)]
pub struct DidDocument {
    pub id: Did,
    pub verification_method_ids: Vec<DidUrl>,
}

#[derive(Copy, Clone, Debug)]
pub enum VerificationRelationship {
    Authentication,
    AssertionMethod,
    KeyAgreement,
    CapabilityInvocation,
    CapabilityDelegation,
}

#[derive(Clone, Debug)]
pub struct DidKeyCreateOptions {
    pub jwk: String,
    pub use_jcs_pub: bool,
}
#[derive(Clone, Debug)]
pub struct DidWebCreateOptions {
    pub domain: String,
    pub path: String,
}
#[derive(Clone, Debug)]
pub struct DidPeerCreateOptions {
    // TODO: implement
}

#[derive(Clone, Debug)]
pub enum DidCreateParams {
    Key(DidKeyCreateOptions),
    Web(DidWebCreateOptions),
    Peer(DidPeerCreateOptions),
}

#[allow(async_fn_in_trait)]
pub trait DidResolver {
    async fn resolve(&self, did: &Did) -> Result<DidDocument, DidError>;
}
#[allow(async_fn_in_trait)]
pub trait DidProvider: Send + Sync {
    fn method(&self) -> DidMethod;
    async fn create(&self, params: &DidCreateParams) -> Result<Did, DidError>;
}

pub trait DidManager: DidProvider + DidResolver {}
impl<T: DidProvider + DidResolver> DidManager for T {}
