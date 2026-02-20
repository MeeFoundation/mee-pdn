use futures_core::{Future, Stream};
use serde::{Deserialize, Serialize};
pub mod error;
pub use error::SyncError;
use std::pin::Pin;

pub use mee_types::{NodeId, TransportUserId};

pub mod managers;

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceId(pub String);

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DirectAddress(pub String);
impl DirectAddress {
    pub fn parse(&self) -> Result<std::net::SocketAddr, std::net::AddrParseError> {
        self.0.parse()
    }
}
impl From<&str> for DirectAddress {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for DirectAddress {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<std::net::SocketAddr> for DirectAddress {
    fn from(addr: std::net::SocketAddr) -> Self {
        Self(addr.to_string())
    }
}
impl std::fmt::Display for DirectAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl AsRef<str> for DirectAddress {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelayEndpoint(pub String);
impl From<&str> for RelayEndpoint {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for RelayEndpoint {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl std::fmt::Display for RelayEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl AsRef<str> for RelayEndpoint {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
    #[serde(default)]
    pub direct_addresses: Vec<DirectAddress>,
    #[serde(default)]
    pub relay_url: Option<RelayEndpoint>,
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubspaceId(pub String);
impl From<&str> for SubspaceId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for SubspaceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl std::fmt::Display for SubspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl AsRef<str> for SubspaceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntryPath(pub String);
impl From<&str> for EntryPath {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for EntryPath {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl std::fmt::Display for EntryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl AsRef<str> for EntryPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTicket {
    // Keep caps opaque so the API is backend-agnostic
    pub caps: Vec<serde_json::Value>,
    pub nodes: Vec<NodeAddr>,
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
    pub subspace_hex: SubspaceId,
    pub path: EntryPath,
    pub payload_len: u64,
}

#[allow(async_fn_in_trait)]
pub trait SyncHandle: Stream<Item = SyncEvent> + Send + Unpin {
    fn complete<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncError>> + Send + 'a>>;
}

#[allow(async_fn_in_trait)]
pub trait SyncEngine: Send + Sync {
    async fn addr(&self) -> Result<NodeAddr, SyncError>;
    async fn user_id(&self) -> Result<TransportUserId, SyncError>;
    async fn create_namespace(&self, owner: &TransportUserId) -> Result<NamespaceId, SyncError>;
    async fn list_namespaces(&self) -> Result<Vec<NamespaceId>, SyncError>;
    async fn share(
        &self,
        ns: &NamespaceId,
        to: &TransportUserId,
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


    type EntryStream: Stream<Item = Result<EntryInfo, SyncError>> + Send + Unpin + 'static;
    async fn get_entries(&self, ns: &NamespaceId) -> Result<Self::EntryStream, SyncError>;
}
