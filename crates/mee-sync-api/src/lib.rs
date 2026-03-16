use futures_core::{Future, Stream};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::pin::Pin;

pub mod error;
pub use error::SyncError;

pub use mee_types::NodeId;

// ---------------------------------------------------------------------------
// Byte-backed IDs (reuse macro from mee-types)
// ---------------------------------------------------------------------------

mee_types::define_byte_id! {
    /// Willow namespace key (32 bytes).
    pub struct NamespaceId;
}

mee_types::define_byte_id! {
    /// Willow subspace / entry-author key (32 bytes).
    ///
    /// This is the operational signing key used for entry authoring,
    /// capability delegation, and subspace ownership. It maps to
    /// Willow's `UserId` concept.
    pub struct SubspaceId;
}

// ---------------------------------------------------------------------------
// Network address types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DirectAddress(
    #[serde(
        serialize_with = "ser_socket_addr",
        deserialize_with = "de_socket_addr"
    )]
    pub std::net::SocketAddr,
);

fn ser_socket_addr<S: serde::Serializer>(
    addr: &std::net::SocketAddr,
    ser: S,
) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&addr.to_string())
}

fn de_socket_addr<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<std::net::SocketAddr, D::Error> {
    let s = <String as Deserialize>::deserialize(de)?;
    s.parse().map_err(serde::de::Error::custom)
}

impl From<std::net::SocketAddr> for DirectAddress {
    fn from(addr: std::net::SocketAddr) -> Self {
        Self(addr)
    }
}

impl fmt::Display for DirectAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelayEndpoint(pub String);
impl From<&str> for RelayEndpoint {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}
impl From<String> for RelayEndpoint {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl fmt::Display for RelayEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl AsRef<str> for RelayEndpoint {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Node address bundle
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
    #[serde(default)]
    pub direct_addresses: Vec<DirectAddress>,
    #[serde(default)]
    pub relay_url: Option<RelayEndpoint>,
}

// ---------------------------------------------------------------------------
// EntryPath — validated Willow path
// ---------------------------------------------------------------------------

/// Error returned when constructing an invalid [`EntryPath`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct PathValidationError {
    pub message: String,
}

/// A validated Willow entry path.
///
/// Components are separated by `/`. Constraints:
/// - No empty components (no leading, trailing, or double slashes)
/// - At most 16 components
/// - Each component at most 256 bytes (UTF-8)
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EntryPath(String);

/// Maximum number of path components.
const MAX_PATH_COMPONENTS: usize = 16;
/// Maximum byte length per component.
const MAX_COMPONENT_BYTES: usize = 256;

impl EntryPath {
    /// Create a new validated entry path.
    pub fn new(path: impl Into<String>) -> Result<Self, PathValidationError> {
        let s: String = path.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    /// View the path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterate over path components.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    fn validate(s: &str) -> Result<(), PathValidationError> {
        if s.is_empty() {
            return Err(PathValidationError {
                message: "path must not be empty".into(),
            });
        }
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() > MAX_PATH_COMPONENTS {
            return Err(PathValidationError {
                message: format!(
                    "too many components: {} (max {})",
                    parts.len(),
                    MAX_PATH_COMPONENTS
                ),
            });
        }
        for part in &parts {
            if part.is_empty() {
                return Err(PathValidationError {
                    message: "empty component (leading, trailing, \
                         or double slash)"
                        .into(),
                });
            }
            if part.len() > MAX_COMPONENT_BYTES {
                return Err(PathValidationError {
                    message: format!(
                        "component too long: {} bytes (max {})",
                        part.len(),
                        MAX_COMPONENT_BYTES
                    ),
                });
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for EntryPath {
    type Error = PathValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl TryFrom<&str> for EntryPath {
    type Error = PathValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<EntryPath> for String {
    fn from(p: EntryPath) -> Self {
        p.0
    }
}

impl fmt::Display for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for EntryPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Sync protocol types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTicket {
    // TODO: Replace opaque serde_json::Value caps with a typed capability
    // representation once meadowcap serialization format stabilises.
    pub caps: Vec<serde_json::Value>,
    /// Connection hints — addresses where the sharer was at ticket
    /// creation time. May be stale or empty; gossip provides fallback.
    pub node_hints: Vec<NodeAddr>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessMode {
    Read,
    Write,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    ReconcileOnce,
    Continuous,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncEvent {
    CapabilityIntersection,
    InterestIntersection,
    Reconciled,
    ReconciledAll,
    Abort { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryInfo {
    pub namespace: NamespaceId,
    pub subspace: SubspaceId,
    pub path: EntryPath,
    pub payload_len: u64,
}

// ---------------------------------------------------------------------------
// Roadmap placeholders
// ---------------------------------------------------------------------------

// TODO(personal-namespaces): Integrate into create_namespace() and
// home_namespace(). Currently defined but unused.
/// Whether a namespace is single-owner or communal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamespaceKind {
    /// Single owner controls all subspaces.
    Owned,
    /// Multiple writers via delegated subspaces.
    Communal,
}

// TODO(personal-namespaces): Integrate into capability delegation.
// Currently defined but unused.
/// A principal's role within a namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamespaceRole {
    Owner,
    Writer,
    Reader,
}

// ---------------------------------------------------------------------------
// Sync engine trait
// ---------------------------------------------------------------------------

#[allow(async_fn_in_trait)]
pub trait SyncHandle: Stream<Item = SyncEvent> + Send + Unpin {
    fn complete<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncError>> + Send + 'a>>;
}

// TODO(personal-namespaces): Add home namespace support:
// - async fn home_namespace(&self) -> Result<NamespaceId, SyncError>;
//   Creates on first call, reloads from persistent store after restart.
// - async fn publish_sys_metadata(&self, path: &str, data: &[u8]) -> ...;
//   Writes to _sys/{path} in home namespace (addrs, kel, peers).
#[allow(async_fn_in_trait)]
pub trait SyncEngine: Send + Sync {
    async fn addr(&self) -> Result<NodeAddr, SyncError>;

    async fn subspace_id(&self) -> Result<SubspaceId, SyncError>;

    async fn create_namespace(&self, owner: &SubspaceId) -> Result<NamespaceId, SyncError>;

    async fn list_namespaces(&self) -> Result<Vec<NamespaceId>, SyncError>;

    async fn share(
        &self,
        ns: &NamespaceId,
        to: &SubspaceId,
        access: AccessMode,
    ) -> Result<SyncTicket, SyncError>;

    async fn import_and_sync(
        &self,
        ticket: SyncTicket,
        mode: SyncMode,
    ) -> Result<Box<dyn SyncHandle>, SyncError>;

    async fn insert(
        &self,
        ns: &NamespaceId,
        path: &EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError>;

    async fn connect_and_share(
        &self,
        ns: &NamespaceId,
        to: &SubspaceId,
        peer_addr: &NodeAddr,
        access: AccessMode,
    ) -> Result<(), SyncError>;

    type EntryStream: Stream<Item = Result<EntryInfo, SyncError>> + Send + Unpin + 'static;
    async fn get_entries(&self, ns: &NamespaceId) -> Result<Self::EntryStream, SyncError>;
}
