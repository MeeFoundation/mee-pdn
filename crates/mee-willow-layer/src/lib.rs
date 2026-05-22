use mee_sync_api::{EntryPath, NamespaceId, NamespaceKind};
use mee_types::{ClaimId, MeeId, NodeId};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
}

/// `UWill` delegation token: UCAN envelope with a single-claim resource and DID principals.
///
/// Field names follow the UCAN v1.0.0-rc.1 Delegation spec.
/// Willow-level addressing (namespace, subspace, path) is NOT exposed here;
/// the iroh-willow fork resolves `res` → concrete Willow leaf internally.
///
/// See `components/pdn-node/uwill.md` for the full specification.
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
