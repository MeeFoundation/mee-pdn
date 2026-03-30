use ed25519_dalek::SigningKey;
use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityRepository, IdentityResolver, IdentityState, Kel,
    KeyAtResult,
};
use mee_types::{Aid, OperationalKey};
use rand_core::{OsRng, TryRngCore};

/// KERI identity manager backed by a real ed25519 keypair.
///
/// On fresh creation the signing key lives in memory.
/// After reload from the repository the signing key is `None`
/// (future: persist via platform secure storage).
pub struct KeriIdentityManager<R: IdentityRepository> {
    _repo: R,
    kel: Kel,
    /// Ed25519 signing key — available after fresh creation, `None` after reload.
    signing_key: Option<SigningKey>,
}

impl<R: IdentityRepository> KeriIdentityManager<R> {
    /// Load identity from the repository, or create a new one if absent.
    pub async fn init(repo: R) -> Result<Self, IdentityError> {
        if let Some(kel) = repo.load_kel().await? {
            Ok(Self {
                _repo: repo,
                kel,
                signing_key: None,
            })
        } else {
            let signing_key = SigningKey::generate(&mut OsRng.unwrap_err());
            let pub_bytes = signing_key.verifying_key().to_bytes();
            let aid = Aid::from_bytes(pub_bytes);
            let kel = Kel {
                aid,
                current_key: OperationalKey::from_bytes(pub_bytes),
                event_seq: 0,
            };
            repo.store_kel(&kel).await?;
            Ok(Self {
                _repo: repo,
                kel,
                signing_key: Some(signing_key),
            })
        }
    }

    /// Whether the in-memory signing key is available.
    ///
    /// `true` after fresh creation, `false` after reload from repository.
    pub fn has_signing_key(&self) -> bool {
        self.signing_key.is_some()
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

        fn with_kel(kel: Kel) -> Self {
            Self {
                kel: Mutex::new(Some(kel)),
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

    // -- init tests --

    #[tokio::test]
    async fn init_creates_valid_aid() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        assert_ne!(*mgr.aid().as_bytes(), [0u8; 32]);
    }

    #[tokio::test]
    async fn init_creates_ed25519_public_key() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        // AID bytes must be a valid ed25519 public key.
        let result = ed25519_dalek::VerifyingKey::from_bytes(mgr.aid().as_bytes());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn init_stores_kel_in_repo() {
        let repo = InMemoryRepo::empty();
        let mgr = KeriIdentityManager::init(repo).await.unwrap();
        // The repo is moved into the manager; access kel via the manager.
        let state = mgr.kel.clone();
        assert_eq!(state.aid, mgr.aid());
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn init_loads_existing_kel() {
        // Pre-populate repo with a known AID.
        let known_aid = Aid::from_bytes([42u8; 32]);
        let kel = Kel {
            aid: known_aid,
            current_key: OperationalKey::from_bytes([42u8; 32]),
            event_seq: 5,
        };
        let mgr = KeriIdentityManager::init(InMemoryRepo::with_kel(kel))
            .await
            .unwrap();
        assert_eq!(mgr.aid(), known_aid);
        assert_eq!(mgr.kel.event_seq, 5);
    }

    #[tokio::test]
    async fn init_has_signing_key_on_fresh_create() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        assert!(mgr.has_signing_key());
    }

    #[tokio::test]
    async fn init_no_signing_key_on_reload() {
        let known_aid = Aid::from_bytes([7u8; 32]);
        let kel = Kel {
            aid: known_aid,
            current_key: OperationalKey::from_bytes([7u8; 32]),
            event_seq: 0,
        };
        let mgr = KeriIdentityManager::init(InMemoryRepo::with_kel(kel))
            .await
            .unwrap();
        assert!(!mgr.has_signing_key());
    }

    #[tokio::test]
    async fn two_fresh_inits_differ() {
        let a = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let b = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        assert_ne!(a.aid(), b.aid());
    }

    // -- resolve / key_at tests --

    #[tokio::test]
    async fn resolve_own_aid_uses_own_key() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let state = mgr.resolve(&mgr.aid()).await.unwrap();
        assert_eq!(state.current_key, mgr.kel.current_key);
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn resolve_peer_aid_uses_placeholder() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let peer = Aid::from_bytes([99u8; 32]);
        let state = mgr.resolve(&peer).await.unwrap();
        assert_eq!(*state.current_key.as_bytes(), [99u8; 32]);
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn key_at_own_aid() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let result = mgr.key_at(&mgr.aid(), 0).await.unwrap();
        assert_eq!(result.key, mgr.kel.current_key);
        assert!(result.current);
        assert!(!result.compromised);
    }

    // -- stub error tests --

    #[tokio::test]
    async fn rotate_key_not_implemented() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let err = mgr.rotate_key(false).await.unwrap_err();
        assert!(matches!(err, IdentityError::Rotation(_)));
    }

    #[tokio::test]
    async fn export_kel_not_implemented() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let err = mgr.export_kel().await.unwrap_err();
        assert!(matches!(err, IdentityError::Other(_)));
    }

    #[tokio::test]
    async fn import_kel_not_implemented() {
        let mgr = KeriIdentityManager::init(InMemoryRepo::empty())
            .await
            .unwrap();
        let err = mgr.import_kel(b"fake").await.unwrap_err();
        assert!(matches!(err, IdentityError::Other(_)));
    }
}
