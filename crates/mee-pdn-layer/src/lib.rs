use mee_sync_api::NamespaceId;
use mee_types::{MeeId, MeeIdentityProof, OperationalKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// What a peer receives when accepting an invite.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub from: MeeId,
    // Transport hints (NodeAddr) — not yet ported into the rebuilt workspace.
    // Signature and expiry also pending.
}

/// Public view of a connection with a peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: ConnectionId,
    pub peer: MeeId,
    pub alias: Option<String>,
    /// Peer's device operational keys that we know about.
    pub peer_devices: Vec<OperationalKey>,
    /// Claims associated with this connection.
    pub claim_ids: Vec<ClaimId>,
}

mee_types::define_byte_id! {
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
    pub holders: Vec<MeeId>,
    pub access: AccessMode,
    /// Wall-clock expiry, unix ms. `None` = no explicit expiry
    pub expires_at: Option<u64>,
}

/// An assertion about a Subject by a Subject. Inseparable bundle of
/// data (`Attribute`) and access semantics (`Capability`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub about: MeeId,
    pub issued_by: MeeId,
    pub proof_of_issued_by: MeeIdentityProof,
    pub attribute: Attribute,
    pub capability: Capability,
}

mee_types::define_byte_id! {
    pub struct ClaimId;
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
    /// Create a fresh identity. -> (`MeeId`, `OperationalKey`)
    ///
    /// Initializes the local identity state and generates the inception
    /// device key.
    CreateIdentity,

    /// Authorize a new device under the current `MeeId`. -> ()
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

    /// List keys currently active under my `MeeId`. -> `NonEmpty`<OperationalKey>
    ActiveDevices,

    // --- Connections -----------------------------------------------------
    /// Create an invite for a peer. -> Invite
    CreateInvite,

    /// Accept an invite. -> Connection
    AcceptInvite { invite: Invite },

    /// List my connections. -> Vec<Connection>
    ListConnections,

    /// Get details of a specific connection. -> Option<Connection>
    GetConnection { peer: MeeId },

    /// Deactivate a connection. -> ()
    ///
    /// Side effect: all delegated claims involving this peer are revoked.
    DeactivateConnection { peer: MeeId },

    // --- Claims ----------------------------------------------------------
    /// Write a claim into the (subject, self) namespace at `path`. -> ()
    ///
    WriteClaim {
        connection_id: ConnectionId,
        claim: Claim,
    },

    /// Get a claim by id. -> Option<Claim>
    GetClaim { claim_id: ClaimId },

    /// Enumerate everything I can see about a specific `MeeId`.
    /// -> Vec<ClaimId>
    ListClaimsAbout { about: MeeId },

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
