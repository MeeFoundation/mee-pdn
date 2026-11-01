//! A payload that crosses the facade in pieces, each a copy no larger than
//! the host makes it, so neither side of the binding holds the payload whole
//! (the `pdn-mobile/host` spec).

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Stream;
use pdn_node::{PayloadReader, PayloadStream, RecordRef};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

use crate::{
    error::{self, PdnError},
    shapes::{self, PodRecord, PodRecordKind},
};

/// The most one [`RecordReader::read`] hands over.
const READ_PIECE_MAX: usize = 1 << 20;

/// Pieces a writer has handed over and the placing has not taken yet.
const PIECES_IN_FLIGHT: usize = 4;

pub(crate) enum Piece {
    Bytes(Bytes),
    End,
}

/// What the placing reads: the pieces in order, and an error once the
/// writer is gone without its end.
struct Pieces {
    pieces: mpsc::Receiver<Piece>,
    ended: bool,
}

impl Stream for Pieces {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        match self.pieces.poll_recv(cx) {
            Poll::Ready(Some(Piece::Bytes(piece))) => Poll::Ready(Some(Ok(piece))),
            Poll::Ready(Some(Piece::End)) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                self.ended = true;
                Poll::Ready(Some(Err(std::io::Error::other(
                    "the writer was released before its end",
                ))))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The sending half of a placing, and the stream the runtime reads it from.
pub(crate) fn pieces() -> (mpsc::Sender<Piece>, PayloadStream) {
    let (sender, receiver) = mpsc::channel(PIECES_IN_FLIGHT);
    let stream = Pieces {
        pieces: receiver,
        ended: false,
    };
    (sender, Box::pin(stream))
}

enum Writing {
    Open {
        pieces: mpsc::Sender<Piece>,
        placing: JoinHandle<anyhow::Result<RecordRef>>,
        written: u64,
    },
    Closed,
}

/// A record being placed from pieces the host writes in order. The record is
/// placed when [`RecordWriter::finish`] answers it; a writer released
/// before then, past its kind's ceiling, or with nothing written places
/// nothing.
#[derive(uniffi::Object)]
pub struct RecordWriter {
    kind: PodRecordKind,
    state: tokio::sync::Mutex<Writing>,
}

impl RecordWriter {
    pub(crate) fn new(
        kind: PodRecordKind,
        pieces: mpsc::Sender<Piece>,
        placing: JoinHandle<anyhow::Result<RecordRef>>,
    ) -> Self {
        Self {
            kind,
            state: tokio::sync::Mutex::new(Writing::Open {
                pieces,
                placing,
                written: 0,
            }),
        }
    }
}

#[uniffi::export]
impl RecordWriter {
    /// The payload's next bytes. A refusal of the placing — the identity no
    /// member, the store failing — is answered here or by `finish`, and
    /// ends the writer.
    pub async fn write(&self, piece: Vec<u8>) -> Result<(), PdnError> {
        let mut state = self.state.lock().await;
        let Writing::Open {
            pieces, written, ..
        } = &mut *state
        else {
            return Err(finished());
        };
        let total = written.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
        if let Err(refused) = shapes::bounded(self.kind, total) {
            abandon(std::mem::replace(&mut *state, Writing::Closed)).await;
            return Err(refused);
        }
        *written = total;
        if piece.is_empty() || pieces.send(Piece::Bytes(piece.into())).await.is_ok() {
            return Ok(());
        }
        // The placing ended before the payload did; its own outcome says why.
        match std::mem::replace(&mut *state, Writing::Closed) {
            Writing::Open { placing, .. } => {
                Err(outcome(placing.await).err().unwrap_or_else(|| {
                    tracing::error!("a placing ended before its payload and placed a record");
                    PdnError::Internal
                }))
            }
            Writing::Closed => Err(finished()),
        }
    }

    /// Place the record from what was written.
    pub async fn finish(&self) -> Result<PodRecord, PdnError> {
        let mut state = self.state.lock().await;
        let Writing::Open {
            pieces,
            placing,
            written,
        } = std::mem::replace(&mut *state, Writing::Closed)
        else {
            return Err(finished());
        };
        if written == 0 {
            abandon(Writing::Open {
                pieces,
                placing,
                written,
            })
            .await;
            return Err(shapes::empty_payload());
        }
        // A placing that already ended answers through its outcome below.
        let _sent = pieces.send(Piece::End).await;
        drop(pieces);
        outcome(placing.await).map(PodRecord::from)
    }
}

/// Ends a placing unwritten and waits for it, so a refusal that answers
/// means nothing was placed.
async fn abandon(state: Writing) {
    if let Writing::Open {
        pieces, placing, ..
    } = state
    {
        drop(pieces);
        if let Ok(Ok(record)) = placing.await {
            tracing::error!("an abandoned placing placed {record:?}");
        }
    }
}

fn finished() -> PdnError {
    PdnError::malformed("a write to a writer that has ended")
}

fn outcome(
    joined: Result<anyhow::Result<RecordRef>, tokio::task::JoinError>,
) -> Result<RecordRef, PdnError> {
    match joined {
        Ok(placed) => placed.map_err(|err| error::table(&err)),
        Err(err) => {
            tracing::error!("a placing did not finish: {err}");
            Err(PdnError::Internal)
        }
    }
}

/// A claim's or an immutable-document's payload, read in order. It holds the
/// payload in the node's store while it lives, so a long read is not cut by
/// the store's collection.
#[derive(uniffi::Object)]
pub struct RecordReader {
    reader: Arc<tokio::sync::Mutex<PayloadReader>>,
    size: u64,
    tokio: Handle,
}

impl RecordReader {
    pub(crate) fn new(reader: PayloadReader, tokio: Handle) -> Self {
        Self {
            size: reader.len(),
            reader: Arc::new(tokio::sync::Mutex::new(reader)),
            tokio,
        }
    }
}

#[uniffi::export]
impl RecordReader {
    /// The payload's length in bytes, known before any is read.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The next bytes, at most `max` and at most 1 MiB; empty once the
    /// payload is read whole.
    pub async fn read(&self, max: u32) -> Result<Vec<u8>, PdnError> {
        let max = usize::try_from(max)
            .unwrap_or(READ_PIECE_MAX)
            .min(READ_PIECE_MAX);
        let reader = Arc::clone(&self.reader);
        let joined = self
            .tokio
            .spawn(async move { reader.lock().await.read(max).await })
            .await;
        match joined {
            Ok(Ok(piece)) => Ok(piece.to_vec()),
            Ok(Err(err)) => Err(error::table(&err)),
            Err(err) => {
                tracing::error!("a read did not finish: {err}");
                Err(PdnError::Internal)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer released before its end ends the stream the placing reads
    /// with an error, so the placing writes nothing.
    #[test]
    fn a_released_writer_ends_its_stream_with_an_error() -> anyhow::Result<()> {
        let tokio = tokio::runtime::Builder::new_current_thread().build()?;
        tokio.block_on(async {
            let (pieces, mut payload) = pieces();
            pieces
                .send(Piece::Bytes(Bytes::from_static(b"half")))
                .await?;
            drop(pieces);
            let first = std::future::poll_fn(|cx| payload.as_mut().poll_next(cx)).await;
            assert!(matches!(first, Some(Ok(piece)) if piece == "half"));
            let second = std::future::poll_fn(|cx| payload.as_mut().poll_next(cx)).await;
            assert!(matches!(second, Some(Err(_))), "{second:?}");
            Ok(())
        })
    }
}
