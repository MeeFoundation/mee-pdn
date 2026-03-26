use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityRepository, IdentityResolver, IdentityState, Kel,
    KeyAtResult,
};
use mee_types::{Aid, OperationalKey};
use std::time::{SystemTime, UNIX_EPOCH};

/// Placeholder KERI identity manager.
pub struct KeriIdentityManager<R: IdentityRepository> {
    _repo: R,
    kel: Kel,
}

impl<R: IdentityRepository> KeriIdentityManager<R> {
    /// Load identity from the repository, or create a new one if absent.
    pub async fn init(repo: R) -> Result<Self, IdentityError> {
        let kel = if let Some(kel) = repo.load_kel().await? { kel } else {
            let aid = Self::create_aid();
            let kel = Kel {
                aid,
                current_key: OperationalKey::from_bytes(*aid.as_bytes()),
                event_seq: 0,
            };
            repo.store_kel(&kel).await?;
            kel
        };
        Ok(Self { _repo: repo, kel })
    }

    /// Generate a new AID (KERI inception).
    ///
    /// Standalone function — does not require a manager instance.
    // TODO(keri): Real implementation must generate ed25519 keypair,
    // create inception event with pre-rotation commitment, and return
    // the AID derived from the inception public key.
    #[allow(clippy::expect_used)]
    pub fn create_aid() -> Aid {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis();
        let mut bytes = [0u8; 32];
        let ms_bytes = ms.to_le_bytes();
        #[allow(clippy::indexing_slicing)]
        bytes[..16].copy_from_slice(&ms_bytes);
        Aid::from_bytes(bytes)
    }
}

impl<R: IdentityRepository> IdentityProvider for KeriIdentityManager<R> {
    fn aid(&self) -> Aid {
        self.kel.aid
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

impl<R: IdentityRepository> KeriIdentityManager<R> {
    /// Resolve the operational key for an AID. Returns the cached key
    /// for our own AID, or a placeholder (AID bytes) for peers.
    fn operational_key_for(&self, aid: &Aid) -> OperationalKey {
        if *aid == self.kel.aid {
            self.kel.current_key
        } else {
            OperationalKey::from_bytes(*aid.as_bytes())
        }
    }
}

impl<R: IdentityRepository> IdentityResolver for KeriIdentityManager<R> {
    // TODO(keri): Real implementation must walk the peer's KEL,
    // verify signatures and hash links, extract current operational key.
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError> {
        Ok(IdentityState {
            aid: *aid,
            current_key: self.operational_key_for(aid),
            event_seq: if *aid == self.kel.aid { self.kel.event_seq } else { 0 },
        })
    }

    // TODO(keri): Real implementation must walk the KEL to find
    // which key was active at at_time and check for later compromise.
    async fn key_at(&self, aid: &Aid, _at_time: u64) -> Result<KeyAtResult, IdentityError> {
        Ok(KeyAtResult {
            key: self.operational_key_for(aid),
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
