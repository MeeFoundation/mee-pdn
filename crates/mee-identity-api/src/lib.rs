mod error;
pub use error::IdentityError;
use mee_types::{Aid, NonEmpty, OperationalKey};
use serde::{Deserialize, Serialize};

/// The verified current state of a KERI identity.
///
/// Resolved from the AID's Key Event Log (KEL).
#[derive(Clone, Debug)]
pub struct IdentityState {
    /// The stable root Autonomic Identifier.
    pub aid: Aid,

    /// Currently active operational keys (one per device).
    ///
    /// In the single-device model this contains a single key.
    /// Multi-device support will allow multiple concurrent keys.
    pub active_keys: NonEmpty<OperationalKey>,

    /// Sequence number of the latest KEL event.
    /// 0 = inception only, 1 = one rotation, etc.
    pub event_seq: u64,
}

/// Provides key rotation and KEL export for the user identity.
///
/// Holder-side operations. Implementors manage the local KERI
/// key material and KEL.
#[allow(async_fn_in_trait)]
pub trait IdentityProvider: Send + Sync {
    /// The node's AID. Set once at inception, never changes.
    fn aid(&self) -> Aid;

    /// This device's operational key (derived from signing key).
    ///
    /// Differs from `IdentityResolver::resolve()` which returns keys
    /// for **all** devices under this AID.
    fn operational_key(&self) -> OperationalKey;

    /// Authorize a new device key under this AID.
    ///
    /// Adds the key to `active_keys` and appends an authorization
    /// event to the KEL. The new device must have generated its own
    /// keypair; only the public key is passed here.
    async fn add_device(&self, new_key: OperationalKey) -> Result<(), IdentityError>;

    /// Revoke a device's operational key.
    ///
    /// Removes the key from `active_keys` and appends a revocation
    /// event to the KEL. Cannot remove the last remaining key.
    async fn remove_device(&self, key: &OperationalKey) -> Result<(), IdentityError>;

    /// Rotate the current operational key.
    ///
    /// Activates the pre-committed next key and commits to a new
    /// next-key hash. Appends the rotation event to the local KEL.
    ///
    /// If `compromised` is true, this is a **recovery rotation**:
    /// the current key is marked as compromised, signaling relying
    /// parties that signatures from the old key after this event
    /// should not be trusted.
    async fn rotate_key(&self, compromised: bool) -> Result<OperationalKey, IdentityError>;

    /// Export the local KEL as opaque bytes for peer exchange.
    ///
    /// The sync layer sends this to peers during the mee-connect/0
    /// handshake. The KEL is also stored at `_sys/kel/self` in the
    /// owner's namespace and replicates via Willow sync.
    async fn export_kel(&self) -> Result<Vec<u8>, IdentityError>;
}

/// Resolves an AID to its current or historical identity state.
///
/// Relying-party operations. Owns the stored KELs for known peers.
/// The sync layer uses this to verify capabilities anchored to
/// operational keys.
#[allow(async_fn_in_trait)]
pub trait IdentityResolver: Send + Sync {
    /// Resolve an AID to its current verified identity state.
    ///
    /// Walks the peer's KEL from inception through all rotation
    /// events, verifying signatures and hash links, and returns
    /// the state designated by the most recent event.
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError>;

    /// Check the current status of a specific key for an AID.
    ///
    /// Walks the peer's KEL to determine whether the key is
    /// currently active, was rotated out, or was marked compromised.
    ///
    /// Returns `IdentityError::NotFound` if the key was never
    /// part of this AID's KEL.
    async fn verify_key(&self, aid: &Aid, key: &OperationalKey)
        -> Result<KeyStatus, IdentityError>;

    /// Import a peer's KEL — verify and store locally.
    ///
    /// Verifies the full event chain. If a KEL for this AID
    /// already exists locally, extends it with new events.
    /// Rejects forks (divergent KELs for the same AID).
    ///
    /// Returns the AID extracted from the inception event.
    async fn import_kel(&self, kel_bytes: &[u8]) -> Result<Aid, IdentityError>;
}

/// Current status of an operational key within an AID's KEL.
///
/// Returned by `IdentityResolver::verify_key()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStatus {
    /// Key is currently active (associated with a live device).
    Active,
    /// Key was rotated out (replaced by a newer key on the same device).
    Rotated {
        /// Unix-seconds timestamp of the rotation event.
        rotated_at: u64,
    },
    /// Key was marked compromised via recovery rotation.
    Compromised {
        /// Unix-seconds timestamp of the compromise event.
        compromised_at: u64,
    },
}

/// Stub Key Event Log.
///
/// Placeholder for the real KERI KEL. Currently holds the minimum
/// state: AID, active operational keys, and event sequence number.
/// Will be replaced with a real event chain (inception + rotation
/// events) when KERI is implemented.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Kel {
    pub aid: Aid,
    /// Currently active operational keys (one per device).
    ///
    /// In the single-device model this contains a single key.
    pub active_keys: NonEmpty<OperationalKey>,
    pub event_seq: u64,
}

/// Persistence interface for identity state.
#[allow(async_fn_in_trait)]
pub trait IdentityRepository: Send + Sync {
    /// Load the local KEL. Returns `None` if no identity exists yet.
    async fn load_kel(&self) -> Result<Option<Kel>, IdentityError>;

    /// Persist the local KEL.
    async fn store_kel(&self, kel: &Kel) -> Result<(), IdentityError>;
}
