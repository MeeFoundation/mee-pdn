//! What the crate's tests share: records, an in-memory record source, and
//! pod-tools of the tests' own, so no test needs an application.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use graph_query_engine::materialize::{
    newest, register, Materialization, Materialized, Node, Subject, ToolOntology, ToolType,
};
use graph_query_engine::record::{Record, RecordSource};
use serde_json::{json, Value};

pub const NOTE: &str = "app:Note:v1";
pub const MESSAGE: &str = "app:Message:v1";
pub const EVENT: &str = "app:CalendarEvent:v1";
pub const PET_MEDS: &str = "mee:PetMedicationRecord:v1";
pub const NO_SUCH_TYPE: &str = "mee:NoSuchType:v1";
pub const SUBJECT: &str = "mee:Subject:v1";

pub const NOTES: &str = "notes";
pub const MEDS: &str = "meds";
pub const CALENDAR: &str = "calendar";
pub const CHAT: &str = "chat";

/// A record of `author`, written `second` seconds after a fixed start, on
/// `<author>-phone`.
pub fn record(key: &str, author: &str, second: i64, bytes: Value) -> Record {
    Record {
        id: format!("{key}.{second}.{author}"),
        key: key.into(),
        author: author.into(),
        at: Utc.timestamp_opt(1_790_000_000 + second, 0).unwrap(),
        device: Some(format!("{author}-phone")),
        bytes: serde_json::to_vec(&bytes).expect("a JSON value serializes"),
    }
}

fn body(record: &Record) -> Value {
    serde_json::from_slice(&record.bytes).unwrap_or(Value::Null)
}

/// A medication record, the n-th of a series: half Clavamox, half Amoxil.
pub fn medication(n: usize) -> Value {
    json!({ "medication": [{
        "active_ingredient": ["Amoxicillin", format!("Ingredient {n}")],
        "brand_name": if n.is_multiple_of(2) { "Clavamox" } else { "Amoxil" },
        "dose_form": "tablet",
        "dosage_amount": { "value": format!("{}.5", 60 + n), "unit": "mg" },
        "administration": { "frequency": "twice a day", "start_date": format!("2026-10-{:02}", 1 + n % 28), "end_date": "2026-11-14" }
    }]})
}

/// The n-th medication record of a series, keyed `claim<n>`, about one of 5
/// subjects, by Alice or Bob in turn.
pub fn med_record(n: usize) -> Record {
    record(
        &format!("claim{n:03}"),
        if n.is_multiple_of(2) { "alice" } else { "bob" },
        n as i64,
        json!({ "subject": { "id": format!("subject-{}", n % 5), "name": format!("Cat {}", n % 5) }, "value": medication(n) }),
    )
}

/// A record source held in memory: its records, a version where it keeps
/// one, and how often it was read.
#[derive(Default)]
pub struct Store {
    records: Mutex<Vec<Record>>,
    version: Mutex<Option<u64>>,
    reads: AtomicUsize,
}

impl Store {
    pub fn new(records: Vec<Record>) -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(records),
            ..Self::default()
        })
    }

    /// A store that keeps a version, which grows with every record added.
    pub fn versioned(records: Vec<Record>) -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(records),
            version: Mutex::new(Some(0)),
            ..Self::default()
        })
    }

    pub fn add(&self, record: Record) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
        if let Some(v) = self
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            *v += 1;
        }
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RecordSource for Store {
    async fn records(&self) -> Result<Vec<Record>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    async fn version(&self) -> Result<Option<u64>, String> {
        Ok(*self
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

fn tool_type(type_id: &str) -> ToolType {
    ToolType {
        type_id: type_id.into(),
        extends: None,
        embeddable: true,
    }
}

fn about(b: &Value) -> Subject {
    let id = b["about"].as_str().unwrap_or("ginger").to_owned();
    let name = b["about_name"].as_str().map_or_else(
        || {
            let mut name = id.clone();
            name[..1].make_ascii_uppercase();
            name
        },
        str::to_owned,
    );
    Subject {
        id,
        name,
        type_id: SUBJECT.into(),
    }
}

/// Notes: the newest record under a key, a note about the subject it names
/// (Ginger where it names none), or nothing where it holds no text.
pub struct Notes;

impl Materialization for Notes {
    fn kind(&self) -> &str {
        NOTES
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: NOTES.into(),
            types: vec![tool_type(NOTE)],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            let b = body(r);
            let text = b["text"].as_str()?;
            Some(Node::new(key, NOTE, json!({ "content": text })).about(about(&b)))
        })
    }
}

/// Medication records: the newest record under a key, a claim about the
/// subject it names, of the type it names where it names one.
pub struct Meds;

impl Materialization for Meds {
    fn kind(&self) -> &str {
        MEDS
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: MEDS.into(),
            types: vec![tool_type(PET_MEDS), tool_type(NO_SUCH_TYPE)],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            let b = body(r);
            let type_id = b["type"].as_str().unwrap_or(PET_MEDS);
            let subject = Subject {
                id: b["subject"]["id"].as_str()?.to_owned(),
                name: b["subject"]["name"].as_str()?.to_owned(),
                type_id: SUBJECT.into(),
            };
            Some(Node::new(key, type_id, b["value"].clone()).about(subject))
        })
    }
}

/// A calendar: each key one event about Ginger, its value the record's.
pub struct Calendar;

impl Materialization for Calendar {
    fn kind(&self) -> &str {
        CALENDAR
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: CALENDAR.into(),
            types: vec![tool_type(EVENT)],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            Some(Node::new(key, EVENT, body(r)).about(about(&Value::Null)))
        })
    }
}

/// A chat: each key one message, about no subject.
pub struct Chat;

impl Materialization for Chat {
    fn kind(&self) -> &str {
        CHAT
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: CHAT.into(),
            types: vec![tool_type(MESSAGE)],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            Some(Node::new(
                key,
                MESSAGE,
                json!({ "content": body(r)["text"] }),
            ))
        })
    }
}

/// Every pod-tool above, registered once: what an application does when it
/// starts.
pub fn register_all() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        register(Arc::new(Notes));
        register(Arc::new(Meds));
        register(Arc::new(Calendar));
        register(Arc::new(Chat));
    });
}

/// A note record under `key`.
pub fn note(key: &str, author: &str, second: i64, text: &str) -> Record {
    record(key, author, second, json!({ "text": text }))
}
