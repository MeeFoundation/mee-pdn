use mee_sync_api::{EntryPath, NamespaceId};
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
// WillowLayer — interface the PDN layer drives
// ---------------------------------------------------------------------------

/// Error returned by [`WillowLayer`] operations.
#[derive(Debug, thiserror::Error)]
pub enum WillowLayerError {
    /// The local node does not control a device key resolvable to `issued_by`.
    #[error("local node is not authorized to write as {issued_by}")]
    NotAuthorized { issued_by: MeeId },

    /// Payload exceeds the willow `max_payload_size` parameter.
    #[error("payload too large: {size} bytes (max {max})")]
    PayloadTooLarge { size: usize, max: usize },

    /// Underlying willow/storage backend reported an error.
    #[error("storage backend error")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Interface the PDN layer drives to read/write entries in the underlying
/// willow store.
///
/// A [`NamespaceId`] is structurally the pair `(about, issued_by)`:
/// `issued_by` is the sole writer/owner; `about` is the subject the entry
/// concerns. Writing into `namespace` does NOT require `namespace.about`'s
/// consent — only local authority over `namespace.issued_by` is checked.
///
/// This authorization model requires the MeeId-aware willow fork;
/// vanilla willow's authorization cannot express MeeId-based write authority.
#[allow(async_fn_in_trait)]
pub trait WillowLayer: Send + Sync {
    /// Insert `payload` at `path` into `namespace`.
    async fn insert_entry(
        &self,
        namespace: &NamespaceId,
        path: &EntryPath,
        payload: &[u8],
    ) -> Result<(), WillowLayerError>;
}
