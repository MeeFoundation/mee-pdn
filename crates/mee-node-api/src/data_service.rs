use mee_sync_api::SyncError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataEntry {
    pub key: String,
    pub value: String,
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

#[allow(async_fn_in_trait)]
pub trait DataService: Send + Sync {
    async fn set(&self, key: &str, value: &str) -> Result<(), DataError>;
    async fn delete(&self, key: &str) -> Result<(), DataError>;
    async fn get(&self, key: &str) -> Result<Option<DataEntry>, DataError>;
    async fn list(&self, prefix: &str) -> Result<Vec<DataEntry>, DataError>;
}
