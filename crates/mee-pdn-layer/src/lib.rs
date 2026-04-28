use mee_sync_api::{EntryPath, NamespaceId};
use mee_types::{MeeId, OperationalKey};
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
    pub peer: MeeId,
    pub alias: Option<String>,
    /// Peer's device operational keys that we know about.
    pub peer_devices: Vec<OperationalKey>,
}

/// Claim locator: (subject, issuer) pair plus path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimLocator {
    pub about: MeeId,
    pub issued_by: MeeId,
    pub path: EntryPath,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capability {}

/// An assertion about a Subject by a Subject. Inseparable bundle of
/// data (`Attribute`) and access semantics (`Capability`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub attribute: Attribute,
    pub capability: Capability,
}

/// Reference to an Identity Context.
///
/// **Placeholder shape.**
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityContextRef {
    pub counterparty: MeeId,
}

/// A `Claim` conditionally shared into another Identity Context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelegatedClaim {
    pub source: ClaimLocator,
    pub target_context: IdentityContextRef,
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
    /// Create a fresh identity. -> (MeeId, OperationalKey)
    ///
    /// Initializes the local identity state and generates the inception
    /// device key.
    CreateIdentity,

    /// Authorize a new device under the current MeeId. -> ()
    ///
    /// The new device has already generated its keypair; only the public
    /// part is passed here.
    AddDevice { new_key: OperationalKey },

    /// Revoke a device. -> ()
    RevokeDevice { key: OperationalKey },

    /// Rotate this device's key. -> OperationalKey (the new one)
    ///
    /// If `compromised = true`, this is a recovery rotation: the old key
    /// is marked as compromised.
    RotateKey { compromised: bool },

    /// List keys currently active under my MeeId. -> NonEmpty<OperationalKey>
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

    /// Close a connection. -> ()
    ///
    /// Side effect: all delegated claims involving this peer are revoked.
    CloseConnection { peer: MeeId },

    // --- Claims ----------------------------------------------------------
    /// Put a claim into the (subject, self) namespace at `path`. -> ()
    ///
    PutClaim {
        subject: MeeId,
        path: EntryPath,
        claim: Claim,
    },

    /// Get a claim at the given locator. -> Option<Claim>
    GetClaim { locator: ClaimLocator },

    /// Enumerate everything I can see about a specific MeeId.
    /// -> Vec<ClaimLocator>
    ListClaimsAbout { about: MeeId },

    /// Enumerate all claims I have authored. -> Vec<ClaimLocator>
    ListMyClaims,

    // --- Delegation ------------------------------------------------------
    /// Delegate a claim into another Identity Context under conditions.
    /// -> DelegatedClaim
    DelegateClaim {
        source: ClaimLocator,
        to: IdentityContextRef,
        conditions: Capability,
    },

    /// List my outgoing delegations to a specific identity context.
    /// -> Vec<DelegatedClaim>
    ListDelegationsTo { context: IdentityContextRef },

    /// List delegations others have made into my contexts.
    /// -> Vec<DelegatedClaim>
    ListIncomingDelegations,

    // --- Discovery / sync (candidate ops) --------------------------------
    /// Sync a namespace once with reachable peers. -> ()
    SyncOnce { namespace: NamespaceId },
}
