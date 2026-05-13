use mee_sync_api::{EntryPath, NamespaceId, NamespaceKind};
use mee_types::{MeeId, NodeId};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WillowCommand {
    Read,
    Write,
    // Delete,
    // Delegate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: u64,
    pub end: Option<u64>,
}

/// Willow geometric area: namespace + path prefix + time range.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Area {
    pub namespace: NamespaceId,
    pub path_prefix: Option<EntryPath>,
    pub times: TimeRange,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
}

/// UWill delegation token: UCAN envelope + Willow `Area` policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UwillCapability {
    pub issuer: MeeId,
    pub audience: MeeId,
    pub area: Area,
    pub commands: Vec<WillowCommand>,
    pub nbf: u64, // wall-clock validity window (independent of entry timestamps)
    pub exp: u64,
    pub nonce: [u8; 12], // 12-byte random
}

mee_types::define_byte_id! {
    /// CID of a UWill delegation — used for revocation references.
    pub struct CapabilityCid;
}

// ---------------------------------------------------------------------------
// Willow/iroh-layer operation AST
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub enum WillowOp {
    CreateNamespace {
        kind: NamespaceKind,
        owner: MeeId,
    },

    InsertEntry {
        namespace: NamespaceId,
        path: EntryPath,
        payload: Vec<u8>,
    },

    Delegate {
        cap: UwillCapability,
    },

    Revoke {
        cap_cid: CapabilityCid,
    },

    Reconcile {
        namespace: NamespaceId,
        peer: NodeAddr,
    },
}
