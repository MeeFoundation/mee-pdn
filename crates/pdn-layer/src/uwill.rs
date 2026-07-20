//! `UWill` capability tokens.
//!
//! The delegation token format shared by every part of the platform that
//! issues, transports, stores, or validates capabilities. The module is
//! transport-independent: it knows nothing about iroh or the data-layer
//! backend, and its types must not leak below the PDN layer — the data
//! layer sees tokens only as opaque payloads plus an injected ingest
//! policy.
//!
//! Pure chain validation (proof-chain verification, expiry, revocation
//! checks) belongs here; the node runtime resolves an entry to the
//! relevant chain and calls into this module for the verdict.
//!
//! See `components/pdn-node/uwill.md` for the full specification.

use pdn_types::{ClaimId, PdnId};
use serde::{Deserialize, Serialize};

/// Commands that a `UWill` capability can grant.
///
/// `Read` MUST be present in every capability.
/// `Write`, `Delete`, `Delegate` are optional.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Read,
    Write,
    Delete,
    Delegate,
}

/// `UWill` delegation token: UCAN envelope with a single-claim resource and DID principals.
///
/// Field names follow the UCAN v1.0.0-rc.1 Delegation spec.
/// Storage-level addressing (namespace, path) is NOT exposed here;
/// resolving `res` to a concrete storage location is the runtime's job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UwillCapability {
    /// Delegator's PdnId-backed DID.
    pub iss: PdnId,
    /// Delegate's PdnId-backed DID.
    pub aud: PdnId,
    /// Namespace owner's PdnId-backed DID.
    pub sub: PdnId,
    /// Granted commands. `Read` MUST always be present; validators MUST reject tokens without it.
    pub cmd: Vec<Command>,
    /// Resource: the single claim this capability grants access to.
    pub res: ClaimId,
    /// Wall-clock validity start (unix ms).
    pub nbf: u64,
    /// Wall-clock validity end (unix ms).
    pub exp: u64,
    /// 12-byte random nonce.
    pub nonce: [u8; 12],
}

pdn_types::define_byte_id! {
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
