//! The PDN layer: the platform surface products consume.
//!
//! Pure domain — no transport, no storage backend. The domain model
//! (claims, connections, delegation), the operation AST ([`PdnOp`]), and
//! the [`uwill`] capability-token module live here; executing operations
//! over a data layer is the job of the (future) node runtime.

use pdn_types::{ClaimId, NamespaceId, OperationalKey, PdnId, PdnIdentityProof};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod uwill;

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// What a peer receives when accepting an invite.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub from: PdnId,
    // Transport hints (NodeAddr) — not yet wired in.
    // Signature and expiry also pending.
}

/// Public view of a connection with a peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: ConnectionId,
    pub peer: PdnId,
    pub alias: Option<String>,
    /// Peer's device operational keys that we know about.
    pub peer_devices: Vec<OperationalKey>,
    /// Claims associated with this connection.
    pub claim_ids: Vec<ClaimId>,
}

pdn_types::define_byte_id! {
    pub struct ConnectionId;
}

// ---------------------------------------------------------------------------
// Domain-model types: Attribute, Capability, Claim, DelegatedClaim
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessMode {
    Read,
    Write,
    // Delete,
    // Delegate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    pub holders: Vec<PdnId>,
    pub access: AccessMode,
    /// Wall-clock expiry, unix ms. `None` = no explicit expiry
    pub expires_at: Option<u64>,
}

/// An assertion about a Subject by a Subject. Inseparable bundle of
/// data (`Attribute`) and access semantics (`Capability`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub about: PdnId,
    pub issued_by: PdnId,
    pub proof_of_issued_by: PdnIdentityProof,
    pub attribute: Attribute,
    pub capability: Capability,
}

/// A `Claim` conditionally shared into another Identity Context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelegatedClaim {
    pub source: ClaimId,
    pub conditions: Capability,
}

// ---------------------------------------------------------------------------
// PDN-layer operation AST
// ---------------------------------------------------------------------------

/// Draft surface of the PDN layer.
///
/// Each variant is one high-level operation. Inputs are its fields; the
/// return type is stated in the doc comment via `-> ...`. If we later
/// choose FT style, this enum maps one-to-one onto a trait.
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

    /// List keys currently active under my `PdnId`. -> `NonEmpty`<OperationalKey>
    ActiveDevices,

    // --- Connections -----------------------------------------------------
    /// Create an invite for a peer. -> Invite
    CreateInvite,

    /// Accept an invite. -> Connection
    AcceptInvite { invite: Invite },

    /// List my connections. -> Vec<Connection>
    ListConnections,

    /// Get details of a specific connection. -> Option<Connection>
    GetConnection { peer: PdnId },

    /// Deactivate a connection. -> ()
    ///
    /// Side effect: all delegated claims involving this peer are revoked.
    DeactivateConnection { peer: PdnId },

    // --- Claims ----------------------------------------------------------
    /// Write a claim into the (subject, self) namespace at `path`. -> ()
    ///
    WriteClaim {
        connection_id: ConnectionId,
        claim: Claim,
    },

    /// Get a claim by id. -> Option<Claim>
    GetClaim { claim_id: ClaimId },

    /// Enumerate everything I can see about a specific `PdnId`.
    /// -> Vec<ClaimId>
    ListClaimsAbout { about: PdnId },

    /// Enumerate all claims I have authored. -> Vec<ClaimId>
    ListMyClaims,

    // --- Delegation ------------------------------------------------------
    /// Delegate a claim into another Identity Context under conditions.
    /// -> `DelegatedClaim`
    DelegateClaim {
        claim_id: ClaimId,
        capability: Capability,
    },

    /// List my outgoing delegations to a specific identity context.
    /// -> Vec<DelegatedClaim>
    ListDelegationsTo,

    /// List delegations others have made into my contexts.
    /// -> Vec<DelegatedClaim>
    ListIncomingDelegations,

    // --- Discovery / sync (candidate ops) --------------------------------
    /// Sync a namespace once with reachable peers. -> ()
    SyncOnce { namespace: NamespaceId },
}
