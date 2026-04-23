use mee_sync_api::{EntryPath, NamespaceId};
use mee_types::{Aid, OperationalKey};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Access mode granted to a capability recipient.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessMode {
    Read,
    Write,
}

/// Which fragment of a namespace a capability covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// The whole namespace.
    Whole,
    /// A path prefix (all entries beneath it).
    Prefix(EntryPath),
    /// A specific entry.
    Entry(EntryPath),
}

/// What a peer receives when accepting an invite.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub from: Aid,
    // Transport hints (NodeAddr) — not yet ported into the rebuilt workspace.
    // Signature and expiry also pending.
}

/// Public view of a connection with a peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub peer: Aid,
    pub alias: Option<String>,
    /// Peer's device operational keys that we know about.
    pub peer_devices: Vec<OperationalKey>,
}

/// Summary of a granted or received capability.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrantInfo {
    pub counterparty: Aid,
    pub namespace: NamespaceId,
    pub scope: Scope,
    pub access: AccessMode,
}

/// Claim locator: (subject, issuer) pair plus path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimLocator {
    pub about: Aid,
    pub issued_by: Aid,
    pub path: EntryPath,
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
    /// Create a fresh identity. -> (Aid, OperationalKey)
    ///
    /// Initializes the local KEL and generates the inception key.
    CreateIdentity,

    /// Authorize a new device under the current Aid. -> ()
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

    /// List keys currently active under my Aid. -> NonEmpty<OperationalKey>
    ActiveDevices,

    // --- Connections -----------------------------------------------------
    /// Create an invite for a peer. -> Invite
    CreateInvite,

    /// Accept an invite. -> Connection
    AcceptInvite { invite: Invite },

    /// List my connections. -> Vec<Connection>
    ListConnections,

    /// Get details of a specific connection. -> Option<Connection>
    GetConnection { peer: Aid },

    /// Close a connection. -> ()
    ///
    /// Side effect: all capabilities granted to this peer are revoked.
    CloseConnection { peer: Aid },

    // --- Claims (statements) ---------------------------------------------
    /// Write a statement into the (about, self) namespace. -> ()
    ///
    /// Degenerate case `about == self`: writing into one's own namespace
    /// (e.g. an r-card). There is no recipient encoded here — visibility
    /// is governed by Grants.
    WriteClaim {
        about: Aid,
        path: EntryPath,
        bytes: Vec<u8>,
    },

    /// Read a statement from a specific (about, issued_by) namespace.
    /// -> Option<Vec<u8>>
    ReadClaim { locator: ClaimLocator },

    /// Enumerate everything I can see about a specific Aid.
    /// -> Vec<ClaimLocator>
    ///
    /// That is, all namespaces where `about = target` and I hold a
    /// read-cap. Example: "show me what people I know are saying about
    /// Carol."
    ListClaimsAbout { about: Aid },

    /// Enumerate all claims I have authored. -> Vec<ClaimLocator>
    ///
    /// That is, namespaces where `issued_by = self`.
    ListMyClaims,

    // --- Capabilities / sharing ------------------------------------------
    /// Grant a peer a capability over a scope within a namespace. -> ()
    Grant {
        peer: Aid,
        namespace: NamespaceId,
        scope: Scope,
        access: AccessMode,
    },

    /// Revoke a previously issued capability. -> ()
    Revoke {
        peer: Aid,
        namespace: NamespaceId,
        scope: Scope,
    },

    /// What I have granted to this peer. -> Vec<GrantInfo>
    ListOutgoingGrants { peer: Aid },

    /// What peers have granted to me. -> Vec<GrantInfo>
    ListIncomingGrants,

    // --- Discovery / sync (candidate ops) --------------------------------
    /// Sync a namespace once with reachable peers. -> ()
    SyncOnce { namespace: NamespaceId },
}
