mod error;
pub use error::IdentityError;
use mee_types::{Aid, OperationalKey};

/// The verified current state of a KERI identity.
///
/// Resolved from the AID's Key Event Log (KEL).
#[derive(Clone, Debug)]
pub struct IdentityState {
    /// The stable root Autonomic Identifier.
    pub aid: Aid,

    /// Current operational signing key.
    /// Maps to Willow `SubspaceId` / iroh-willow `UserId`.
    pub current_key: OperationalKey,

    /// Sequence number of the latest KEL event.
    /// 0 = inception only, 1 = one rotation, etc.
    pub event_seq: u64,
}

/// Result of a historical key lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyAtResult {
    /// The operational key that was active at the queried time.
    pub key: OperationalKey,
    /// Whether this key is still the current operational key.
    pub current: bool,
    /// Whether this key was later marked compromised
    /// by a recovery rotation.
    pub compromised: bool,
}

/// Provides identity creation, key rotation, and KEL export.
///
/// Holder-side operations. Implementors manage the local KERI
/// key material and KEL.
#[allow(async_fn_in_trait)]
pub trait IdentityProvider: Send + Sync {
    /// Create a new identity (KERI inception event).
    ///
    /// Generates an ed25519 keypair, creates the inception event
    /// with a pre-rotation commitment, and stores the inception
    /// event as the first entry in the local KEL.
    async fn create(&self) -> Result<Aid, IdentityError>;

    /// The node's AID. Set once at inception, never changes.
    fn aid(&self) -> Aid;

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

    /// What key was active for an AID at a point in time?
    ///
    /// Returns the operational key that was active at `at_time`
    /// (Unix seconds), whether it's still current, and whether
    /// it was later marked compromised by a recovery rotation.
    ///
    /// Returns `IdentityError::NotFound` if the AID is unknown
    /// or did not exist at `at_time`.
    async fn key_at(&self, aid: &Aid, at_time: u64) -> Result<KeyAtResult, IdentityError>;

    /// Import a peer's KEL — verify and store locally.
    ///
    /// Verifies the full event chain. If a KEL for this AID
    /// already exists locally, extends it with new events.
    /// Rejects forks (divergent KELs for the same AID).
    ///
    /// Returns the AID extracted from the inception event.
    async fn import_kel(&self, kel_bytes: &[u8]) -> Result<Aid, IdentityError>;
}
