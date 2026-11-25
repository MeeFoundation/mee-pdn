//! What the engine reads a zone's records through: one [`RecordSource`] per zone, implemented by
//! the application, whose records the engine reads only to hand them to that zone's
//! materialization.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// One record of a pod-tool, as the record store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Unique within the store; breaks a tie between two records of a key
    /// written at the same time.
    pub id: String,
    /// The key the pod-tool chose. The newest record under a key stands.
    pub key: String,
    /// Who wrote it, as the host recorded.
    pub author: String,
    pub at: DateTime<Utc>,
    /// The device of the author's it was written on, where the host knows.
    pub device: Option<String>,
    pub bytes: Vec<u8>,
}

impl Record {
    /// The bytes as text, where the pod-tool stored text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// One zone's records, as the host hands them to the engine.
#[async_trait]
pub trait RecordSource: Send + Sync {
    /// Every record of the zone, oldest first.
    async fn records(&self) -> Result<Vec<Record>, String>;

    /// A number that grows whenever anything is written in the zone's Pod,
    /// where the host can tell without reading it; `None` where it cannot.
    /// A zone whose source is at the version it was last caught up with is
    /// not read again.
    async fn version(&self) -> Result<Option<u64>, String> {
        Ok(None)
    }
}
