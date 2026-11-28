//! The materialization interface: what a pod-tool's records stand for in the graph, the nodes and
//! relations a key's records make, handed over oldest first, and the tool ontology that declares
//! their types and relations. The registry starts empty; every pod-tool registers its own. Spec:
//! the materialization-interface capability of mia-docs' `graph-query-engine`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::Record;

/// Who or what a claim is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub id: String,
    pub name: String,
    pub type_id: String,
}

/// One node a key's records stand for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Unique within the pod-tool's zone.
    pub id: String,
    /// A type the pod-tool's tool ontology declares.
    pub type_id: String,
    /// A value of the type's shape.
    pub value: Value,
    /// The subject a claim is about. A node that names none is no claim.
    pub subject: Option<Subject>,
    /// The members of the Pod the node is for. Empty: every member who may
    /// read the zone. A node for some members alone is built only into
    /// their graphs.
    pub readers: Vec<String>,
    /// The record of its key the node takes its member, time and device
    /// from, by record id. `None`: the newest record under its key.
    pub record: Option<String>,
}

impl Node {
    /// A node about no subject, for every member, from the newest record.
    pub fn new(id: impl Into<String>, type_id: impl Into<String>, value: Value) -> Self {
        Self {
            id: id.into(),
            type_id: type_id.into(),
            value,
            subject: None,
            readers: Vec::new(),
            record: None,
        }
    }

    /// The same node, about `subject`: a claim.
    #[must_use]
    pub fn about(mut self, subject: Subject) -> Self {
        self.subject = Some(subject);
        self
    }

    /// The same node, for `members` alone.
    #[must_use]
    pub fn for_members(mut self, members: Vec<String>) -> Self {
        self.readers = members;
        self
    }

    /// The same node, taking its member, time and device from the record
    /// `record_id`.
    #[must_use]
    pub fn from_record(mut self, record_id: impl Into<String>) -> Self {
        self.record = Some(record_id.into());
        self
    }
}

/// A relation from a node a key's records make to a node of the same Pod,
/// in the pod-tool's own zone or another pod-tool's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    /// A name the tool ontology declares.
    pub name: String,
    /// A node the same records make, by id.
    pub from: String,
    /// The pod-tool whose zone the target is in. `None`: this one's.
    pub to_tool: Option<String>,
    /// The target node, by id.
    pub to: String,
}

impl Relation {
    /// A relation to a node of the same zone.
    pub fn new(name: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            from: from.into(),
            to_tool: None,
            to: to.into(),
        }
    }

    /// A relation to a node of another pod-tool's zone.
    pub fn into_zone(
        name: impl Into<String>,
        from: impl Into<String>,
        tool: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            from: from.into(),
            to_tool: Some(tool.into()),
            to: to.into(),
        }
    }
}

/// What one key's records stand for in the graph. Empty: nothing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Materialized {
    pub nodes: Vec<Node>,
    pub relations: Vec<Relation>,
}

impl Materialized {
    pub fn nothing() -> Self {
        Self::default()
    }

    /// One node, and no relation.
    pub fn node(node: Node) -> Self {
        Self {
            nodes: vec![node],
            relations: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.relations.is_empty()
    }
}

/// One type a pod-tool's zone holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolType {
    pub type_id: String,
    /// For a custom type, the platform type it extends, where it extends
    /// one. A platform type extends whatever the platform ontology says it
    /// does, and leaves this empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Worth embedding, so the app can search it by meaning.
    #[serde(default)]
    pub embeddable: bool,
}

/// One relation a pod-tool's zone holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRelation {
    pub name: String,
    /// The type it leads from, one of the zone's own.
    pub from_type: String,
    /// The type it leads to, or one that extends it.
    pub to_type: String,
}

/// What a pod-tool's zone holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOntology {
    /// The pod-tool's kind: its zone's name in every Pod.
    pub kind: String,
    pub types: Vec<ToolType>,
    #[serde(default)]
    pub relations: Vec<ToolRelation>,
}

impl ToolOntology {
    pub fn embeddable(&self, type_id: &str) -> bool {
        self.types
            .iter()
            .any(|t| t.type_id == type_id && t.embeddable)
    }

    pub fn declares_type(&self, type_id: &str) -> bool {
        self.types.iter().any(|t| t.type_id == type_id)
    }

    pub fn relation(&self, name: &str) -> Option<&ToolRelation> {
        self.relations.iter().find(|r| r.name == name)
    }
}

/// A pod-tool's read side.
pub trait Materialization: Send + Sync {
    /// The pod-tool's kind, whose records this reads.
    fn kind(&self) -> &str;

    fn ontology(&self) -> ToolOntology;

    /// What the records under `key` stand for, handed over oldest first by
    /// time and then by record id. It depends on the records alone, never on
    /// the device, the reader or the time it runs.
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized;
}

/// The pod-tools whose data reaches the graph: those registered
/// ([`register`]).
pub fn materializations() -> Vec<Arc<dyn Materialization>> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Add a pod-tool's read side, replacing any registered for its kind: how a
/// pod-tool registers its materialization and tool ontology. Applies to
/// every database from its next catch-up.
pub fn register(materialization: Arc<dyn Materialization>) {
    let mut all = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    all.retain(|m| m.kind() != materialization.kind());
    all.push(materialization);
}

fn registry() -> &'static std::sync::RwLock<Vec<Arc<dyn Materialization>>> {
    use std::sync::{OnceLock, RwLock};
    static ALL: OnceLock<RwLock<Vec<Arc<dyn Materialization>>>> = OnceLock::new();
    ALL.get_or_init(|| RwLock::new(Vec::new()))
}

/// The materialization of `kind`, where one is registered.
pub fn materialization(kind: &str) -> Option<Arc<dyn Materialization>> {
    materializations().into_iter().find(|m| m.kind() == kind)
}

/// `records` by key, in the byte order of the keys, each key's records
/// oldest first by time and then by record id: how they are handed to a
/// materialization.
pub fn by_key(records: &[Record]) -> BTreeMap<String, Vec<Record>> {
    let mut out: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for record in records {
        out.entry(record.key.clone())
            .or_default()
            .push(record.clone());
    }
    for records in out.values_mut() {
        records.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
    }
    out
}

/// A pod-tool that takes the newest record under each key and makes one
/// node of it, or nothing: the commonest materialization, written once.
pub fn newest(records: &[Record], one: impl Fn(&Record) -> Option<Node>) -> Materialized {
    match records.last().and_then(one) {
        Some(node) => Materialized::node(node),
        None => Materialized::nothing(),
    }
}
