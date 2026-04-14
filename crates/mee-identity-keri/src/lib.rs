pub use ed25519_dalek::SigningKey;
use mee_identity_api::{
    IdentityError, IdentityProvider, IdentityRepository, IdentityResolver, IdentityState, Kel,
    KeyStatus,
};
use mee_types::{Aid, NonEmpty, OperationalKey};
use rand_core::{OsRng, TryRngCore};
use std::sync::Mutex;

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
        active_keys: NonEmpty::new(OperationalKey::from_bytes(pub_bytes)),
        event_seq: 0,
    };
    (kel, signing_key)
}

/// KERI identity manager backed by a real ed25519 keypair.
///
/// A node always has both a KEL and a signing key. The caller
/// provides both at construction time — identity creation and
/// key retrieval happen outside the manager.
///
/// The KEL is behind a `Mutex` to allow mutation via `&self`
/// (required by `IdentityProvider` trait methods).
pub struct KeriIdentityManager<R: IdentityRepository> {
    repo: R,
    /// Cached AID — set at inception, never changes.
    aid: Aid,
    kel: Mutex<Kel>,
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
        let aid = kel.aid;
        Self {
            repo,
            aid,
            kel: Mutex::new(kel),
            signing_key,
        }
    }

    /// Access the signing key.
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

#[allow(clippy::expect_used)] // Mutex lock — unrecoverable if poisoned
impl<R: IdentityRepository> IdentityProvider for KeriIdentityManager<R> {
    fn aid(&self) -> Aid {
        self.aid
    }

    fn operational_key(&self) -> OperationalKey {
        OperationalKey::from_bytes(self.signing_key.verifying_key().to_bytes())
    }

    async fn add_device(&self, new_key: OperationalKey) -> Result<(), IdentityError> {
        // Mutate under lock, clone, then drop lock before .await.
        let snapshot = {
            let mut kel = self.kel.lock().expect("kel lock poisoned");
            if kel.active_keys.contains(&new_key) {
                return Err(IdentityError::Invalid(
                    "key already in active_keys".to_owned(),
                ));
            }
            kel.active_keys.push(new_key);
            kel.event_seq += 1;
            kel.clone()
        };
        self.repo.store_kel(&snapshot).await
    }

    async fn remove_device(&self, key: &OperationalKey) -> Result<(), IdentityError> {
        let snapshot = {
            let mut kel = self.kel.lock().expect("kel lock poisoned");
            kel.active_keys.try_remove(|k| k == key).map_err(|()| {
                IdentityError::Invalid("key not found or cannot remove last key".to_owned())
            })?;
            kel.event_seq += 1;
            kel.clone()
        };
        self.repo.store_kel(&snapshot).await
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

#[allow(clippy::expect_used)] // Mutex lock — unrecoverable if poisoned
impl<R: IdentityRepository> IdentityResolver for KeriIdentityManager<R> {
    // TODO(keri): Real implementation must walk the peer's KEL,
    // verify signatures and hash links, extract current operational key.
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError> {
        let kel = self.kel.lock().expect("kel lock poisoned");
        Ok(IdentityState {
            aid: *aid,
            active_keys: if *aid == self.aid {
                kel.active_keys.clone()
            } else {
                NonEmpty::new(OperationalKey::from_bytes(*aid.as_bytes()))
            },
            event_seq: if *aid == self.aid { kel.event_seq } else { 0 },
        })
    }

    // TODO(keri): Real implementation must walk the KEL to find
    // which event introduced/rotated/compromised this key.
    async fn verify_key(
        &self,
        aid: &Aid,
        key: &OperationalKey,
    ) -> Result<KeyStatus, IdentityError> {
        if *aid == self.aid {
            let kel = self.kel.lock().expect("kel lock poisoned");
            if kel.active_keys.iter().any(|k| k == key) {
                Ok(KeyStatus::Active)
            } else {
                Err(IdentityError::NotFound(
                    "key not found in active keys".to_owned(),
                ))
            }
        } else {
            // Stub for peers: if key matches AID bytes, treat as active
            if *key == OperationalKey::from_bytes(*aid.as_bytes()) {
                Ok(KeyStatus::Active)
            } else {
                Err(IdentityError::NotFound("unknown peer key".to_owned()))
            }
        }
    }

    // TODO(keri): Real implementation must verify the KEL chain
    // and store it locally for future resolve/verify_key calls.
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
        assert_eq!(*kel.active_keys.first().as_bytes(), pub_bytes);
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

    // -- resolve / verify_key tests --

    #[tokio::test]
    async fn resolve_own_aid_uses_own_key() {
        let mgr = make_manager();
        let state = mgr.resolve(&mgr.aid()).await.unwrap();
        assert_eq!(
            *state.active_keys.first().as_bytes(),
            mgr.signing_key().verifying_key().to_bytes()
        );
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn resolve_peer_aid_uses_placeholder() {
        let mgr = make_manager();
        let peer = Aid::from_bytes([99u8; 32]);
        let state = mgr.resolve(&peer).await.unwrap();
        assert_eq!(*state.active_keys.first().as_bytes(), [99u8; 32]);
        assert_eq!(state.event_seq, 0);
    }

    #[tokio::test]
    async fn verify_key_own_active() {
        let mgr = make_manager();
        let key = mgr.operational_key();
        let status = mgr.verify_key(&mgr.aid(), &key).await.unwrap();
        assert_eq!(status, KeyStatus::Active);
    }

    #[tokio::test]
    async fn verify_key_unknown_returns_not_found() {
        let mgr = make_manager();
        let unknown = OperationalKey::from_bytes([77u8; 32]);
        let err = mgr.verify_key(&mgr.aid(), &unknown).await.unwrap_err();
        assert!(matches!(err, IdentityError::NotFound(_)));
    }

    // -- operational_key tests --

    #[test]
    fn operational_key_matches_signing_key() {
        let mgr = make_manager();
        let expected = mgr.signing_key().verifying_key().to_bytes();
        assert_eq!(*mgr.operational_key().as_bytes(), expected);
    }

    // -- add_device / remove_device tests --

    #[tokio::test]
    async fn add_device_adds_key() {
        let mgr = make_manager();
        let new_key = OperationalKey::from_bytes([42u8; 32]);
        mgr.add_device(new_key).await.unwrap();

        let state = mgr.resolve(&mgr.aid()).await.unwrap();
        assert_eq!(state.active_keys.len(), 2);
        assert_eq!(state.event_seq, 1);
    }

    #[tokio::test]
    async fn add_device_rejects_duplicate() {
        let mgr = make_manager();
        let own_key = mgr.operational_key();
        let err = mgr.add_device(own_key).await.unwrap_err();
        assert!(matches!(err, IdentityError::Invalid(_)));
    }

    #[tokio::test]
    async fn remove_device_removes_key() {
        let mgr = make_manager();
        let new_key = OperationalKey::from_bytes([42u8; 32]);
        mgr.add_device(new_key).await.unwrap();
        mgr.remove_device(&new_key).await.unwrap();

        let state = mgr.resolve(&mgr.aid()).await.unwrap();
        assert_eq!(state.active_keys.len(), 1);
        assert_eq!(state.event_seq, 2);
    }

    #[tokio::test]
    async fn remove_device_rejects_last_key() {
        let mgr = make_manager();
        let own_key = mgr.operational_key();
        let err = mgr.remove_device(&own_key).await.unwrap_err();
        assert!(matches!(err, IdentityError::Invalid(_)));
    }

    #[tokio::test]
    async fn add_device_persists_to_repo() {
        let mgr = make_manager();
        let new_key = OperationalKey::from_bytes([42u8; 32]);
        mgr.add_device(new_key).await.unwrap();

        let stored = mgr.repo.load_kel().await.unwrap().unwrap();
        assert_eq!(stored.active_keys.len(), 2);
        assert_eq!(stored.event_seq, 1);
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
