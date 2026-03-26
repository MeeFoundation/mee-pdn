use thiserror::Error;

/// Errors from the identity subsystem.
#[derive(Error, Debug)]
pub enum IdentityError {
    #[error("identity creation error: {0}")]
    Creation(String),
    #[error("identity resolve error: {0}")]
    Resolve(String),
    #[error("identity not found: {0}")]
    NotFound(String),
    #[error("invalid identity data: {0}")]
    Invalid(String),
    // TODO(keri): Implement once KEL parsing and chain verification
    // exist. Should carry structured info about which event failed
    // and why (bad signature, missing predecessor, etc.).
    #[error("KEL verification error: {0}")]
    KelVerification(String),
    #[error("key rotation error: {0}")]
    Rotation(String),
    #[error("KEL conflict: logs diverge at event {0}")]
    KelConflict(u64),
    #[error("other identity error: {0}")]
    Other(String),
}
