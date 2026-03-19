use thiserror::Error;

/// Errors from the gossip discovery subsystem.
#[derive(Debug, Error)]
pub enum GossipError {
    #[error("gossip protocol error: {0}")]
    Protocol(String),

    #[error("advertisement serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("invalid advertisement: {0}")]
    InvalidAdvertisement(String),

    #[error("signature verification failed")]
    SignatureVerification,

    #[error("sync error: {0}")]
    Sync(#[from] mee_sync_api::SyncError),
}
