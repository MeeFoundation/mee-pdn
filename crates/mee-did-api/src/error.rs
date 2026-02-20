use thiserror::Error;

#[derive(Error, Debug)]
pub enum DidError {
    #[error("DID method error: {0}")]
    Method(String),
    #[error("DID resolve error: {0}")]
    Resolve(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Invalid: {0}")]
    Invalid(String),
    #[error("Other: {0}")]
    Other(String),
}
