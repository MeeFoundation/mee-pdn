//! `UWill` capability tokens.
//!
//! The delegation token format shared by every layer that issues,
//! transports, stores, or validates capabilities. This crate is
//! transport-independent: it knows nothing about iroh, willow, or how
//! tokens travel between nodes.
//!
//! Pure chain validation (proof-chain verification, expiry, revocation
//! checks) belongs here as it lands; backends such as the iroh-docs
//! ingest gate only resolve an entry to the relevant chain and call into
//! this crate for the verdict.
//!
//! See `components/pdn-node/uwill.md` for the full specification.

use mee_types::{ClaimId, MeeId};
use serde::{Deserialize, Serialize};

/// Commands that a `UWill` capability can grant.
///
/// `Read` MUST be present in every capability.
/// `Write`, `Delete`, `Delegate` are optional.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WillowCommand {
    Read,
    Write,
    Delete,
    Delegate,
}

/// `UWill` delegation token: UCAN envelope with a single-claim resource and DID principals.
///
/// Field names follow the UCAN v1.0.0-rc.1 Delegation spec.
/// Willow-level addressing (namespace, subspace, path) is NOT exposed here;
/// the sync backend resolves `res` → concrete storage leaf internally.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UwillCapability {
    /// Delegator's MeeId-backed DID.
    pub iss: MeeId,
    /// Delegate's MeeId-backed DID.
    pub aud: MeeId,
    /// Namespace owner's MeeId-backed DID.
    pub sub: MeeId,
    /// Granted commands. `Read` MUST always be present; validators MUST reject tokens without it.
    pub cmd: Vec<WillowCommand>,
    /// Resource: the single claim this capability grants access to.
    pub res: ClaimId,
    /// Wall-clock validity start (unix ms).
    pub nbf: u64,
    /// Wall-clock validity end (unix ms).
    pub exp: u64,
    /// 12-byte random nonce.
    pub nonce: [u8; 12],
}

mee_types::define_byte_id! {
    /// CID of a `UWill` delegation — used for revocation references.
    pub struct CapabilityCid;
}

/// Wall-clock validity window for a `UWill` capability.
///
/// Both bounds are absolute unix-ms timestamps, matching the wire format
/// of `nbf` / `exp` in [`UwillCapability`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ValidityWindow {
    pub nbf: u64,
    pub exp: u64,
}
