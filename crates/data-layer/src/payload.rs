//! A payload that crosses in pieces: imported into the blob store from a
//! stream before its entry is written, and read back from it the same way,
//! so neither direction holds a payload whole in memory (the `pod-store`
//! spec of `data-layer`).

use std::pin::Pin;

use anyhow::Result;
use bytes::Bytes;
use futures_core::Stream;
use iroh_blobs::api::{blobs::BlobReader, TempTag};
use tokio::io::AsyncReadExt as _;

/// What a streamed payload is read from: its bytes in order, an error ending
/// it unwritten.
pub type PayloadStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send + Sync>>;

/// A payload whole in the blob store and held there, no entry naming it yet:
/// what [`SyncNode::write_pod_record_imported`](crate::SyncNode::write_pod_record_imported)
/// writes. Dropped unwritten, it is left to blob collection.
#[derive(Debug)]
pub struct ImportedPayload {
    pub(crate) tag: TempTag,
    pub(crate) len: u64,
}

impl ImportedPayload {
    /// Its length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it holds no byte: the engine keeps no entry for such a payload.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A record's payload read in order, held against blob collection for as
/// long as the reader lives.
#[derive(Debug)]
pub struct PayloadReader {
    reader: BlobReader,
    len: u64,
    _held: TempTag,
}

impl PayloadReader {
    pub(crate) fn new(reader: BlobReader, len: u64, held: TempTag) -> Self {
        Self {
            reader,
            len,
            _held: held,
        }
    }

    /// The payload's length in bytes, known before any is read.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the payload holds no byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The next bytes, at most `max`; empty once the payload is read whole.
    pub async fn read(&mut self, max: usize) -> Result<Bytes> {
        let mut buffer = vec![0; max];
        let mut filled = 0;
        while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
            let read = self.reader.read(rest).await?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        Ok(buffer.into())
    }
}
