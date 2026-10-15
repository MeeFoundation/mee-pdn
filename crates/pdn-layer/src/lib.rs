//! The PDN layer: the platform surface products consume.
//!
//! Pure domain — no transport, no storage backend. The domain model
//! (claims and their attributes) and the operation AST ([`PdnOp`]) live
//! here. A draft: nothing in the workspace consumes it, and nothing
//! executes a [`PdnOp`].

use std::collections::BTreeMap;

use pdn_types::{ClaimId, OperationalKey, PdnId, PdnIdentityProof};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain-model types: Attribute, Claim
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AttributeValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<AttributeValue>),
    Set(Vec<AttributeValue>),
    Object(BTreeMap<String, AttributeValue>),
}

/// Named, typed property holding a single piece of data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
}

/// An assertion about a Subject by a Subject, carrying its data
/// (`Attribute`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub about: PdnId,
    pub issued_by: PdnId,
    pub proof_of_issued_by: PdnIdentityProof,
    pub attribute: Attribute,
}

// ---------------------------------------------------------------------------
// PDN-layer operation AST
// ---------------------------------------------------------------------------

/// Draft surface of the PDN layer.
///
/// Each variant is one high-level operation. Inputs are its fields; the
/// return type is stated in the doc comment via `-> ...`.
#[allow(dead_code)]
pub enum PdnOp {
    // --- Identity and devices --------------------------------------------
    /// Create a fresh identity. -> (`PdnId`, `OperationalKey`)
    ///
    /// Initializes the local identity state and generates the inception
    /// device key.
    CreateIdentity,

    /// Authorize a new device under the current `PdnId`. -> ()
    ///
    /// The new device has already generated its keypair; only the public
    /// part is passed here.
    AddDevice { new_key: OperationalKey },

    /// Revoke a device. -> ()
    RevokeDevice { key: OperationalKey },

    /// Rotate this device's key. -> `OperationalKey` (the new one)
    ///
    /// If `compromised = true`, this is a recovery rotation: the old key
    /// is marked as compromised.
    RotateKey { compromised: bool },

    /// List keys currently active under my `PdnId`, never none.
    /// -> Vec<OperationalKey>
    ActiveDevices,

    // --- Claims ----------------------------------------------------------
    /// Write a claim (addressed by its `ClaimId`). -> ()
    WriteClaim { claim: Claim },

    /// Get a claim by id. -> Option<Claim>
    GetClaim { claim_id: ClaimId },

    /// Enumerate everything I can see about a specific `PdnId`.
    /// -> Vec<ClaimId>
    ListClaimsAbout { about: PdnId },

    /// Enumerate all claims I have authored. -> Vec<ClaimId>
    ListMyClaims,
}
