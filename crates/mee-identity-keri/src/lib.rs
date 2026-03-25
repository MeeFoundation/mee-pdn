use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityResolver, IdentityState, KeyAtResult,
};
use mee_types::{Aid, OperationalKey};
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
#[allow(clippy::expect_used)]
fn timestamp_aid() -> Aid {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis();
    let mut bytes = [0u8; 32];
    let ms_bytes = ms.to_le_bytes();
    // REASON: ms_bytes is 16 bytes, bytes is 32 — safe slice copy
    #[allow(clippy::indexing_slicing)]
    bytes[..16].copy_from_slice(&ms_bytes);
    Aid::from_bytes(bytes)
}

#[allow(clippy::expect_used)]
impl IdentityProvider for KeriIdentityManager {
    // TODO(keri): Real implementation must generate ed25519 keypair,
    // create inception event with pre-rotation commitment, store in KEL.
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

    // TODO(keri): Real implementation must activate the pre-committed
    // next key and append a rotation event to the KEL.
    async fn rotate_key(&self, _compromised: bool) -> Result<OperationalKey, IdentityError> {
        Err(IdentityError::Rotation(
            "key rotation not yet implemented".to_owned(),
        ))
    }

    // TODO(keri): Real implementation must serialize the local KEL
    // (inception event + all rotation events) for peer exchange.
    async fn export_kel(&self) -> Result<Vec<u8>, IdentityError> {
        Err(IdentityError::Other(
            "KEL export not yet implemented".to_owned(),
        ))
    }
}

impl IdentityResolver for KeriIdentityManager {
    // TODO(keri): Real implementation must walk the peer's KEL,
    // verify signatures and hash links, extract current operational key.
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError> {
        // Placeholder: operational key = AID bytes (no key rotation)
        Ok(IdentityState {
            aid: *aid,
            current_key: OperationalKey::from_bytes(*aid.as_bytes()),
            event_seq: 0,
        })
    }

    // TODO(keri): Real implementation must walk the KEL to find
    // which key was active at at_time and check for later compromise.
    async fn key_at(&self, aid: &Aid, _at_time: u64) -> Result<KeyAtResult, IdentityError> {
        // Placeholder: only one key (the AID), always current, never compromised
        Ok(KeyAtResult {
            key: OperationalKey::from_bytes(*aid.as_bytes()),
            current: true,
            compromised: false,
        })
    }

    // TODO(keri): Real implementation must verify the KEL chain
    // and store it locally for future resolve/key_at calls.
    async fn import_kel(&self, _kel_bytes: &[u8]) -> Result<Aid, IdentityError> {
        Err(IdentityError::Other(
            "KEL import not yet implemented".to_owned(),
        ))
    }
}
