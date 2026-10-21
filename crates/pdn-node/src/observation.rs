//! The observation surface behind `test-util`: one subscription per hosted
//! identity and pod, numbered records over both stores' ordered
//! subscriptions, and the record listing. The observation spec of
//! `pdn-node` holds the contract; `pdn-app/sync-test/plan.md` the record's
//! fields.

use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::SystemTime,
};

use data_layer::{
    ContentStatus, LiveEvent, OrderedEvents, Origin, PodObservation, RecordEntry, SyncEvent,
    Verdict,
};
use futures_lite::StreamExt as _;
use pdn_types::PodId;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

/// Each store's ordered subscription holds this many events for the
/// numbering task's scheduling delay: a 1,000-entry session's at most 2,002.
pub(crate) const STORE_CAPACITY: usize = 4_096;

/// Holds a 100,000-record catch-up's first session, at most 200,002 events,
/// beside the membership store's.
pub(crate) const RECORD_BUFFER_BOUND: usize = 262_144;

/// One numbered record of a [`PodSubscription`], in the one JSON form every
/// host writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodRecord {
    pub sub: String,
    pub seq: u64,
    /// Microseconds since the Unix epoch when the runtime took the event off
    /// the store's ordered subscription.
    pub t: u64,
    pub pod: String,
    pub store: ObservedStore,
    #[serde(flatten)]
    pub body: RecordBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedStore {
    Membership,
    Records,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordBody {
    InsertLocal {
        key: String,
        entry_ts: u64,
        hash: String,
        len: u64,
    },
    InsertRemote {
        key: String,
        entry_ts: u64,
        hash: String,
        len: u64,
        from: String,
        content_status: ContentStatusText,
    },
    ContentReady {
        hash: String,
    },
    PendingContentReady,
    SyncFinished {
        peer: String,
        origin: SessionOrigin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        started: u64,
        finished: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        received: Option<u64>,
    },
    Lagged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentStatusText {
    Complete,
    Incomplete,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    Connect,
    Accept,
}

/// One entry of the record listing, in the one JSON form every host
/// writes; `payload` only when present, base64 in JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedEntry {
    pub key: String,
    pub author: String,
    pub hash: String,
    pub len: u64,
    pub present: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "base64_payload"
    )]
    pub payload: Option<Vec<u8>>,
    /// `counted`, `not_yet`, `for_nothing` or `outside_layout`.
    pub verdict: String,
    /// What a verdict other than `counted` names, in snake case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

mod base64_payload {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{de::Error as _, Deserialize as _, Deserializer, Serializer};

    #[allow(clippy::ref_option)] // serde's `with` passes the field by reference
    pub fn serialize<S: Serializer>(payload: &Option<Vec<u8>>, to: S) -> Result<S::Ok, S::Error> {
        match payload {
            Some(bytes) => to.serialize_some(&STANDARD.encode(bytes)),
            None => to.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(from)?
            .map(|text| STANDARD.decode(text).map_err(D::Error::custom))
            .transpose()
    }
}

impl ListedEntry {
    pub(crate) fn new(entry: &RecordEntry, verdict: Verdict, payload: Option<Vec<u8>>) -> Self {
        let (verdict, reason) = match verdict {
            Verdict::Counted => ("counted", None),
            Verdict::NotYet(awaiting) => ("not_yet", Some(snake_case(&format!("{awaiting:?}")))),
            Verdict::CountedForNothing(why) => {
                ("for_nothing", Some(snake_case(&format!("{why:?}"))))
            }
            Verdict::OutsideLayout => ("outside_layout", None),
        };
        Self {
            key: String::from_utf8_lossy(&entry.key).into_owned(),
            author: entry.author.to_string(),
            hash: entry.hash.to_string(),
            len: entry.len,
            present: entry.payload.is_some(),
            payload,
            verdict: verdict.to_owned(),
            reason,
        }
    }
}

fn snake_case(camel: &str) -> String {
    let mut snake = String::with_capacity(camel.len() + 4);
    for (index, c) in camel.char_indices() {
        if c.is_ascii_uppercase() && index > 0 {
            snake.push('_');
        }
        snake.push(c.to_ascii_lowercase());
    }
    snake
}

/// The consumer's end: reads the oldest record waiting. Dropping it ends
/// the numbering task.
pub struct PodSubscription {
    sub: String,
    records: mpsc::UnboundedReceiver<PodRecord>,
    waiting: Arc<AtomicUsize>,
    room: Arc<Notify>,
}

impl PodSubscription {
    pub fn id(&self) -> &str {
        &self.sub
    }

    /// `None` once the subscription has ended — the runtime stopped, or
    /// both stores closed — and every record before it was read.
    pub async fn next(&mut self) -> Option<PodRecord> {
        let record = self.records.recv().await?;
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        self.room.notify_one();
        Some(record)
    }
}

/// A subscription identifier new to it.
pub(crate) fn fresh_sub() -> anyhow::Result<u64> {
    use anyhow::Context as _;
    use rand::TryRng as _;

    rand::rngs::SysRng
        .try_next_u64()
        .context("operating-system randomness unavailable")
}

/// Spawn the numbering task over `observation` and answer its consumer's
/// end.
pub(crate) fn subscribe(
    pod: PodId,
    observation: PodObservation,
    stopping: CancellationToken,
    bound: usize,
    sub: u64,
) -> PodSubscription {
    let (tx, records) = mpsc::unbounded_channel();
    let waiting = Arc::new(AtomicUsize::new(0));
    let room = Arc::new(Notify::new());
    let sub = format!("{sub:016x}");
    let pump = Pump {
        sub: sub.clone(),
        pod: pod.to_string(),
        seq: 0,
        tx,
        waiting: Arc::clone(&waiting),
        room: Arc::clone(&room),
        bound,
        lost_membership: false,
        lost_records: false,
    };
    tokio::spawn(pump.run(observation, stopping));
    PodSubscription {
        sub,
        records,
        waiting,
        room,
    }
}

struct Pump {
    sub: String,
    pod: String,
    seq: u64,
    tx: mpsc::UnboundedSender<PodRecord>,
    /// Records sent and not yet read: incremented before the send, so never
    /// below what the channel holds.
    waiting: Arc<AtomicUsize>,
    room: Arc<Notify>,
    bound: usize,
    /// Per store, whether records were dropped at the bound since the last
    /// `lagged` it yielded.
    lost_membership: bool,
    lost_records: bool,
}

impl Pump {
    async fn run(mut self, observation: PodObservation, stopping: CancellationToken) {
        let PodObservation {
            membership,
            mut records,
        } = observation;
        let mut membership = Some(membership);
        if records.is_none()
            && !self.admit(ObservedStore::Records, now_micros(), RecordBody::Closed)
        {
            return;
        }
        loop {
            if membership.is_none() && records.is_none() && !self.lagging() {
                return;
            }
            let (store, event) = tokio::select! {
                biased;
                () = stopping.cancelled() => return,
                () = self.tx.closed() => return,
                () = self.room.notified(), if self.lagging() => {
                    if !self.flush_lags() {
                        return;
                    }
                    continue;
                }
                event = next_of(&mut membership) => (ObservedStore::Membership, event),
                event = next_of(&mut records) => (ObservedStore::Records, event),
            };
            let t = now_micros();
            let body = match event {
                Some(Ok(event)) => match body_of(event) {
                    Some(body) => body,
                    None => continue,
                },
                // An event the store's stream failed to carry is lost.
                Some(Err(_lost)) => RecordBody::Lagged,
                None => {
                    match store {
                        ObservedStore::Membership => membership = None,
                        ObservedStore::Records => records = None,
                    }
                    RecordBody::Closed
                }
            };
            if !self.admit(store, t, body) {
                return;
            }
        }
    }

    fn lagging(&self) -> bool {
        self.lost_membership || self.lost_records
    }

    fn lost(&mut self, store: ObservedStore) -> &mut bool {
        match store {
            ObservedStore::Membership => &mut self.lost_membership,
            ObservedStore::Records => &mut self.lost_records,
        }
    }

    /// `false` once the consumer is gone.
    fn admit(&mut self, store: ObservedStore, t: u64, body: RecordBody) -> bool {
        if !self.flush_lags() {
            return false;
        }
        if self.waiting.load(Ordering::Acquire) >= self.bound {
            if body != RecordBody::Closed {
                *self.lost(store) = true;
                return true;
            }
            // A store's end goes past the bound, behind the `lagged` of what
            // it lost: a consumer reading late must still learn the store
            // ended.
            if std::mem::take(self.lost(store)) && !self.send(store, t, RecordBody::Lagged) {
                return false;
            }
        }
        self.send(store, t, body)
    }

    /// One `lagged` per store that lost records, while there is room.
    fn flush_lags(&mut self) -> bool {
        for store in [ObservedStore::Membership, ObservedStore::Records] {
            if !*self.lost(store) || self.waiting.load(Ordering::Acquire) >= self.bound {
                continue;
            }
            *self.lost(store) = false;
            if !self.send(store, now_micros(), RecordBody::Lagged) {
                return false;
            }
        }
        true
    }

    fn send(&mut self, store: ObservedStore, t: u64, body: RecordBody) -> bool {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        let record = PodRecord {
            sub: self.sub.clone(),
            seq: self.seq,
            t,
            pod: self.pod.clone(),
            store,
            body,
        };
        if self.tx.send(record).is_err() {
            return false;
        }
        self.seq += 1;
        true
    }
}

async fn next_of(events: &mut Option<OrderedEvents>) -> Option<anyhow::Result<LiveEvent>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// `None` for a neighbour event, which is dropped before numbering.
fn body_of(event: LiveEvent) -> Option<RecordBody> {
    Some(match event {
        LiveEvent::InsertLocal { entry } => RecordBody::InsertLocal {
            key: String::from_utf8_lossy(entry.key()).into_owned(),
            entry_ts: entry.timestamp(),
            hash: entry.content_hash().to_string(),
            len: entry.content_len(),
        },
        LiveEvent::InsertRemote {
            from,
            entry,
            content_status,
        } => RecordBody::InsertRemote {
            key: String::from_utf8_lossy(entry.key()).into_owned(),
            entry_ts: entry.timestamp(),
            hash: entry.content_hash().to_string(),
            len: entry.content_len(),
            from: from.to_string(),
            content_status: match content_status {
                ContentStatus::Complete => ContentStatusText::Complete,
                ContentStatus::Incomplete => ContentStatusText::Incomplete,
                ContentStatus::Missing => ContentStatusText::Missing,
            },
        },
        LiveEvent::ContentReady { hash } => RecordBody::ContentReady {
            hash: hash.to_string(),
        },
        LiveEvent::PendingContentReady => RecordBody::PendingContentReady,
        LiveEvent::SyncFinished(event) => session_body(event),
        LiveEvent::Lagged => RecordBody::Lagged,
        LiveEvent::NeighborUp(_) | LiveEvent::NeighborDown(_) => return None,
    })
}

fn session_body(event: SyncEvent) -> RecordBody {
    let (ok, error, sent, received) = match event.result {
        Ok(details) => (
            true,
            None,
            u64::try_from(details.entries_sent).ok(),
            u64::try_from(details.entries_received).ok(),
        ),
        Err(error) => (false, Some(error), None, None),
    };
    RecordBody::SyncFinished {
        peer: event.peer.to_string(),
        origin: match event.origin {
            Origin::Connect(_) => SessionOrigin::Connect,
            Origin::Accept => SessionOrigin::Accept,
        },
        session: event.session.map(|id| {
            id.iter().fold(String::with_capacity(32), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
        }),
        started: micros(event.started),
        finished: micros(event.finished),
        ok,
        error,
        sent,
        received,
    }
}

fn micros(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

fn now_micros() -> u64 {
    micros(SystemTime::now())
}

#[cfg(test)]
mod tests;
