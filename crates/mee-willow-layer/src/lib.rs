use futures_core::Stream;
use mee_sync_api::{EntryInfo, EntryPath, NamespaceId};
use mee_types::{ClaimId, MeeId, NodeId};
use serde::{Deserialize, Serialize};

// Token format and (future) chain validation live in the transport-agnostic
// `uwill` crate; re-exported here because the [`WillowLayer`] trait speaks
// in these types.
pub use uwill::{CapabilityCid, UwillCapability, ValidityWindow, WillowCommand};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
}

// ---------------------------------------------------------------------------
// WillowLayer — interface the PDN layer drives
// ---------------------------------------------------------------------------

/// Error returned by [`WillowLayer`] operations.
#[derive(Debug, thiserror::Error)]
pub enum WillowLayerError {
    /// The local node does not control a device key resolvable to `issued_by`.
    #[error("local node is not authorized to write as {issued_by}")]
    NotAuthorizedToWrite { issued_by: MeeId },

    /// The local node is neither the namespace owner of `res` nor a holder
    /// of a parent capability for `res` that includes the `Delegate` command.
    #[error("local node is not authorized to delegate {res}")]
    NotAuthorizedToDelegate { res: ClaimId },

    /// Requested commands are not a subset of what the holder can delegate
    /// (either a parent capability does not cover them, or `Read` is missing).
    #[error("requested commands exceed holder's delegatable set: {requested:?}")]
    CommandsExceedAuthority { requested: Vec<WillowCommand> },

    /// Requested validity window is not contained within the parent
    /// capability's `[nbf, exp]`.
    #[error("requested validity {requested:?} not within parent's {parent:?}")]
    ValidityOutsideParent {
        requested: ValidityWindow,
        parent: ValidityWindow,
    },

    /// Local capability store has no record for `cid` — cannot build the
    /// proof chain needed to verify revocation authority.
    #[error("capability not found: {cid}")]
    CapabilityNotFound { cid: CapabilityCid },

    /// The local node's `MeeId` is not in the issuer chain of `cid` up to
    /// the namespace owner.
    #[error("local node is not authorized to revoke {cid}")]
    NotAuthorizedToRevoke { cid: CapabilityCid },

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

    /// Issue a `UWill` capability granting `commands` over `res` to `audience`,
    /// valid during `validity`.
    ///
    /// Succeeds when the local node is either the owner of the namespace
    /// containing `res`, or holds a parent capability for `res` whose `cmd`
    /// includes `Delegate` and covers all of `commands`. In the latter case
    /// the parent capability is embedded in the proof chain automatically.
    ///
    /// Returns the signed capability (for transmission to `audience`) and
    /// its CID (for local tracking and future revocation).
    async fn issue_capability(
        &self,
        audience: MeeId,
        commands: Vec<WillowCommand>,
        res: ClaimId,
        validity: ValidityWindow,
    ) -> Result<(UwillCapability, CapabilityCid), WillowLayerError>;

    /// Revoke the capability identified by `cid`.
    ///
    /// Succeeds when the local node's `MeeId` is in the issuer chain of
    /// `cid` up to the namespace owner. Idempotent: revoking an already
    /// revoked capability is a no-op `Ok(())`.
    ///
    /// Revocation records propagate via willow sync; callers do not need
    /// to distribute anything themselves.
    ///
    /// Consider returning the revocation `Invocation`'s CID if a future
    /// caller needs to reference it (e.g. for audit). Not exposed today.
    async fn revoke_capability(&self, cid: CapabilityCid) -> Result<(), WillowLayerError>;

    /// Read the payload bytes for the entry at `path` in `namespace`.
    /// Returns `Ok(None)` if no such entry exists.
    async fn get_entry(
        &self,
        namespace: &NamespaceId,
        path: &EntryPath,
    ) -> Result<Option<Vec<u8>>, WillowLayerError>;

    /// Stream type yielding entry metadata for [`list_entries`].
    type EntryStream: Stream<Item = Result<EntryInfo, WillowLayerError>> + Send + Unpin + 'static;

    /// Enumerate metadata for entries in `namespace`, optionally filtered
    /// to those whose `path` starts with `path_prefix`.
    ///
    /// Yields metadata only (no payload bytes); use [`get_entry`] to fetch
    /// payloads for entries of interest.
    async fn list_entries(
        &self,
        namespace: &NamespaceId,
        path_prefix: Option<&EntryPath>,
    ) -> Result<Self::EntryStream, WillowLayerError>;
}
