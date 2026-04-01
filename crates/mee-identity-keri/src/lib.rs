pub use ed25519_dalek::SigningKey;
use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityRepository, IdentityResolver, IdentityState, Kel,
    KeyAtResult,
};
use mee_types::{Aid, OperationalKey};
use rand_core::{OsRng, TryRngCore};

/// Generate a new ed25519 identity (KERI inception).
///
/// Returns the initial KEL and the signing key. The caller is
/// responsible for persisting both: KEL via `IdentityRepository`,
/// signing key via secure storage.
pub fn create_identity() -> (Kel, SigningKey) {
    let signing_key = SigningKey::generate(&mut OsRng.unwrap_err());
    let pub_bytes = signing_key.verifying_key().to_bytes();
    let aid = Aid::from_bytes(pub_bytes);
    let kel = Kel {
        aid,
        current_key: OperationalKey::from_bytes(pub_bytes),
        event_seq: 0,
    };
    (kel, signing_key)
}

/// KERI identity manager backed by a real ed25519 keypair.
///
/// A node always has both a KEL and a signing key. The caller
/// provides both at construction time — identity creation and
/// key retrieval happen outside the manager.
pub struct KeriIdentityManager<R: IdentityRepository> {
    _repo: R,
    kel: Kel,
    signing_key: SigningKey,
}

impl<R: IdentityRepository> KeriIdentityManager<R> {
    /// Create a new identity manager with the given KEL and signing key.
    ///
    /// The caller is responsible for:
    /// - Loading or creating the KEL (see [`create_identity`])
    /// - Providing the signing key from secure storage or fresh generation
    /// - Persisting both before calling this constructor
    pub fn new(repo: R, kel: Kel, signing_key: SigningKey) -> Self {
        Self {
            _repo: repo,
            kel,
            signing_key,
        }
    }

    /// Access the signing key.
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
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
            event_seq: if *aid == self.kel.aid {
                self.kel.event_seq
            } else {
                0
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use mee_identity_api::Kel;
    use std::sync::Mutex;

    /// In-memory repository for tests (avoids Willow/iroh dependency).
    struct InMemoryRepo {
        kel: Mutex<Option<Kel>>,
    }

    impl InMemoryRepo {
        fn empty() -> Self {
            Self {
                kel: Mutex::new(None),
            }
        }
    }

    #[allow(clippy::expect_used)]
    impl IdentityRepository for InMemoryRepo {
        async fn load_kel(&self) -> Result<Option<Kel>, IdentityError> {
            Ok(self.kel.lock().expect("test lock poisoned").clone())
        }

        async fn store_kel(&self, kel: &Kel) -> Result<(), IdentityError> {
            *self.kel.lock().expect("test lock poisoned") = Some(kel.clone());
            Ok(())
        }
    }

    fn make_manager() -> KeriIdentityManager<InMemoryRepo> {
        let (kel, signing_key) = create_identity();
        KeriIdentityManager::new(InMemoryRepo::empty(), kel, signing_key)
    }

    // -- create_identity tests --

    #[test]
    fn create_identity_valid_aid() {
        let (kel, _) = create_identity();
        assert_ne!(*kel.aid.as_bytes(), [0u8; 32]);
    }

    #[test]
    fn create_identity_ed25519_public_key() {
        let (kel, _) = create_identity();
        let result = ed25519_dalek::VerifyingKey::from_bytes(kel.aid.as_bytes());
        assert!(result.is_ok());
    }

    #[test]
    fn create_identity_kel_matches_signing_key() {
        let (kel, signing_key) = create_identity();
        let pub_bytes = signing_key.verifying_key().to_bytes();
        assert_eq!(*kel.aid.as_bytes(), pub_bytes);
        assert_eq!(*kel.current_key.as_bytes(), pub_bytes);
        assert_eq!(kel.event_seq, 0);
    }

    #[test]
    fn create_identity_unique() {
        let (a, _) = create_identity();
        let (b, _) = create_identity();
        assert_ne!(a.aid, b.aid);
    }

    // -- new() tests --

    #[test]
    fn new_returns_correct_aid() {
        let mgr = make_manager();
        let expected = mgr.signing_key().verifying_key().to_bytes();
        assert_eq!(*mgr.aid().as_bytes(), expected);
    }

    // -- resolve / key_at tests --

    #[tokio::test]
    async fn resolve_own_aid_uses_own_key() {
        let mgr = make_manager();
        let state = mgr.resolve(&mgr.aid()).await.unwrap();
        assert_eq!(state.current_key, mgr.kel.current_key);
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn resolve_peer_aid_uses_placeholder() {
        let mgr = make_manager();
        let peer = Aid::from_bytes([99u8; 32]);
        let state = mgr.resolve(&peer).await.unwrap();
        assert_eq!(*state.current_key.as_bytes(), [99u8; 32]);
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn key_at_own_aid() {
        let mgr = make_manager();
        let result = mgr.key_at(&mgr.aid(), 0).await.unwrap();
        assert_eq!(result.key, mgr.kel.current_key);
        assert!(result.current);
        assert!(!result.compromised);
    }

    // -- stub error tests --

    #[tokio::test]
    async fn rotate_key_not_implemented() {
        let mgr = make_manager();
        let err = mgr.rotate_key(false).await.unwrap_err();
        assert!(matches!(err, IdentityError::Rotation(_)));
    }

    #[tokio::test]
    async fn export_kel_not_implemented() {
        let mgr = make_manager();
        let err = mgr.export_kel().await.unwrap_err();
        assert!(matches!(err, IdentityError::Other(_)));
    }

    #[tokio::test]
    async fn import_kel_not_implemented() {
        let mgr = make_manager();
        let err = mgr.import_kel(b"fake").await.unwrap_err();
        assert!(matches!(err, IdentityError::Other(_)));
    }
}
