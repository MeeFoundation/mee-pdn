use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Invalid namespace: {0}")]
    InvalidNamespace(String),
    #[error("Invalid identifier: {0}")]
    InvalidId(String),
    #[error("Address parse error: {0}")]
    AddrParse(#[from] std::net::AddrParseError),
    #[error("Backend error: {0}")]
    Backend(String),
    #[error("Other: {0}")]
    Other(String),
}
