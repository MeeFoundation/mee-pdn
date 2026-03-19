use mee_sync_api::{NamespaceId, SyncError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataEntry {
    pub key: String,
    pub value: Vec<u8>,
}

/// Domain errors for the data service layer.
#[derive(Debug, Error)]
pub enum DataError {
    /// Error propagated from the sync engine.
    #[error("sync error: {0}")]
    Sync(#[from] SyncError),
    /// Requested key does not exist.
    #[error("not found: {key}")]
    NotFound { key: String },
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Data operations on a specific Willow namespace.
///
/// Every method takes an explicit `NamespaceId`. Callers pass
/// `node.home_namespace()` for their own data, or any other
/// namespace they have access to.
#[allow(async_fn_in_trait)]
pub trait DataService: Send + Sync {
    async fn set(&self, ns: &NamespaceId, key: &str, value: &[u8]) -> Result<(), DataError>;
    async fn delete(&self, ns: &NamespaceId, key: &str) -> Result<(), DataError>;
    async fn get(&self, ns: &NamespaceId, key: &str) -> Result<Option<DataEntry>, DataError>;
    async fn list(&self, ns: &NamespaceId, prefix: &str) -> Result<Vec<DataEntry>, DataError>;
}
