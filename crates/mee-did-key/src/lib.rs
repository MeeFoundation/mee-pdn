use mee_did_api::{DidCreateParams, DidDocument, DidError, DidProvider, DidResolver};
use mee_types::{Did, DidMethod, DidUrl};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct KeyDidManager;

#[allow(clippy::expect_used)]
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis()
}

impl DidResolver for KeyDidManager {
    async fn resolve(&self, did: &Did) -> Result<DidDocument, DidError> {
        // placeholder, returns minimal doc with one verification method id
        Ok(DidDocument {
            id: did.clone(),
            verification_method_ids: vec![DidUrl(format!("{did}#key-1"))],
        })
    }
}

impl DidProvider for KeyDidManager {
    fn method(&self) -> DidMethod {
        DidMethod::Key
    }

    async fn create(&self, params: &DidCreateParams) -> Result<Did, DidError> {
        match params {
            DidCreateParams::Key(opts) => {
                // placeholder, encode whether jcs-pub was requested
                let suffix = if opts.use_jcs_pub { "jcs" } else { "raw" };
                Ok(Did(format!("did:key:z{}-{}", now_ms(), suffix)))
            }
            _ => Err(DidError::Method(
                "unsupported create params for did:key".into(),
            )),
        }
    }
}
