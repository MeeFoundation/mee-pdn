mod error;
pub use error::IdentityError;
use mee_types::Aid;

/// The verified current state of a KERI identity.
///
/// Resolved from the AID's Key Event Log (KEL).
/// Contains the stable root identifier plus the current
/// operational signing key.
#[derive(Clone, Debug)]
pub struct IdentityState {
    /// The stable root Autonomic Identifier.
    /// Derived from the ed25519 inception public key.
    pub aid: Aid,

    /// Current operational signing key (`SubspaceId` in Willow terms).
    /// Designated by the most recent rotation or inception event.
    // TODO(keri): This must be verified against the KEL chain.
    // Currently set to the same bytes as the AID (no key rotation
    // support yet). Real implementation extracts this from the
    // latest rotation event, or from inception if no rotation.
    pub current_operational_key: [u8; 32],
}

/// Provides identity creation and access to the node's AID.
///
/// Implementors manage the local KERI key material and KEL.
#[allow(async_fn_in_trait)]
pub trait IdentityProvider: Send + Sync {
    /// Create a new identity (KERI inception event).
    ///
    /// Generates a new ed25519 keypair, creates the inception
    /// event, and returns the resulting AID.
    // TODO(keri): Real implementation must:
    // 1. Generate ed25519 keypair
    // 2. Derive AID from inception public key
    // 3. Create and sign inception event with pre-rotation
    //    key commitment
    // 4. Store inception event in local KEL
    // 5. Set initial operational key = inception key
    async fn create(&self) -> Result<Aid, IdentityError>;

    /// Return the node's AID. Set once at inception, never changes.
    fn aid(&self) -> Aid;
}

/// Resolves an AID to its current verified identity state.
///
/// In KERI direct mode, this means obtaining and verifying the
/// peer's Key Event Log.
#[allow(async_fn_in_trait)]
pub trait IdentityResolver: Send + Sync {
    /// Resolve an AID to its current identity state.
    // TODO(keri): Real implementation must:
    // 1. Obtain the peer's KEL (from local cache or via
    //    mee-connect/0 direct exchange)
    // 2. Verify the full event chain (signatures, hash links)
    // 3. Extract current operational key from the latest
    //    rotation event (or inception if no rotation)
    // 4. Return verified IdentityState
    async fn resolve(&self, aid: &Aid) -> Result<IdentityState, IdentityError>;
}
