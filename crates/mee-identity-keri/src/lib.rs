use mee_identity_api::{IdentityError, IdentityProvider, IdentityResolver, IdentityState};
use mee_types::Aid;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Placeholder KERI identity manager.
///
/// Currently generates timestamp-based AIDs and stores the identity
/// in memory. Every method carries a `// TODO(keri):` comment
/// describing what the real implementation must do.
pub struct KeriIdentityManager {
    // TODO(keri): Replace with real key material:
    // - ed25519 signing keypair (inception key)
    // - pre-rotation key commitment
    // - local KEL (Key Event Log) storage
    aid: Mutex<Option<Aid>>,
}

impl KeriIdentityManager {
    pub fn new() -> Self {
        Self {
            aid: Mutex::new(None),
        }
    }
}

impl Default for KeriIdentityManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a placeholder AID from the current timestamp.
// TODO(keri): Replace with real ed25519 key generation.
// Real implementation: generate ed25519 keypair, derive AID
// from the public key bytes (the inception key).
#[allow(clippy::expect_used)]
fn timestamp_aid() -> Aid {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis();
    let mut bytes = [0u8; 32];
    let ms_bytes = ms.to_le_bytes();
    // REASON: ms_bytes is 16 bytes, bytes is 32 — copy_from_slice is safe
    // for the first 16 bytes.
    #[allow(clippy::indexing_slicing)]
    bytes[..16].copy_from_slice(&ms_bytes);
    Aid::from_bytes(bytes)
}

#[allow(clippy::expect_used)]
impl IdentityProvider for KeriIdentityManager {
    // TODO(keri): Real implementation must:
    // 1. Generate ed25519 keypair
    // 2. Derive AID from inception public key
    // 3. Create and sign inception event with pre-rotation
    //    key commitment
    // 4. Store inception event in local KEL
    // 5. Set initial operational key = inception key
    async fn create(&self) -> Result<Aid, IdentityError> {
        let aid = timestamp_aid();
        let mut guard = self.aid.lock().expect("aid lock poisoned");
        *guard = Some(aid);
        Ok(aid)
    }

    fn aid(&self) -> Aid {
        self.aid
            .lock()
            .expect("aid lock poisoned")
            .unwrap_or_else(|| Aid::from_bytes([0u8; 32]))
    }
}

impl IdentityResolver for KeriIdentityManager {
    // TODO(keri): Real implementation must:
    // 1. Obtain the peer's KEL (from local cache or via
    //    mee-connect/0 direct exchange)
    // 2. Verify the full event chain (signatures, hash links)
    // 3. Extract current operational key from the latest
    //    rotation event (or inception if no rotation)
    // 4. Return verified IdentityState
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError> {
        // Placeholder: operational key = AID bytes (no key rotation)
        Ok(IdentityState {
            aid: *aid,
            current_operational_key: *aid.as_bytes(),
        })
    }
}
