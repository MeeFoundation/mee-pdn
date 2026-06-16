//! The data vocabulary: how entries and namespaces are addressed across
//! the platform. Shared by the data layer (which stores and syncs them)
//! and the PDN layer (which speaks about them) without either depending
//! on the other.

use crate::{NodeId, PdnId};
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// NamespaceId
// ---------------------------------------------------------------------------

/// Data namespace identifier — the pair `(about, issued_by)`.
///
/// `issued_by` is the sole writer/owner; `about` is the subject the
/// namespace's entries concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceId {
    pub about: PdnId,
    pub issued_by: PdnId,
}

impl NamespaceId {
    pub const fn new(about: PdnId, issued_by: PdnId) -> Self {
        Self { about, issued_by }
    }
}

// ---------------------------------------------------------------------------
// EntryPath — validated entry path
// ---------------------------------------------------------------------------

/// Error returned when constructing an invalid [`EntryPath`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct PathValidationError {
    pub message: String,
}

/// A validated entry path.
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
// EntryInfo — metadata about a stored entry, without the payload bytes
// ---------------------------------------------------------------------------

/// Metadata for a single data-layer entry, without the payload bytes.
///
/// Returned by enumeration methods so callers can decide which entries'
/// payloads to actually load.
///
/// The author dimension is omitted: in our model it is fixed to
/// `namespace.issued_by` (so claims converge across the issuer's devices
/// via the data layer's newer-wins overwrite semantics) and would be
/// redundant here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryInfo {
    pub namespace: NamespaceId,
    pub path: EntryPath,
    pub payload_len: u64,
}

// ---------------------------------------------------------------------------
// Namespace roles
// ---------------------------------------------------------------------------

/// A principal's role within a namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamespaceRole {
    Owner,
    Writer,
    Reader,
}

// ---------------------------------------------------------------------------
// Transport hints
// ---------------------------------------------------------------------------

/// Transport address hint for reaching a node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddr {
    pub node_id: NodeId,
}
