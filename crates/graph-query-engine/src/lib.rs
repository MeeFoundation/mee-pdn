//! The graph query engine, the only way to the graph database (ADR-0019): pod-tools reflect their
//! records into the graph through a registered [`materialize::Materialization`], readers query it
//! through handles fixed to the pods and zones they were made for ([`PodGraph`], [`GraphView`]),
//! and an owner searches texts by meaning through a [`vector::VectorIndex`]. `LadybugDB`, its query
//! language and its schema stay behind these interfaces. Spec: mia-docs, `components/mee-pdn/graph-
//! query-engine`.

pub mod conform;
pub mod db;
pub mod materialize;
pub mod ontology;
pub mod query;
pub mod record;
pub mod shape;
pub mod vector;

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use conform::{check, conform};
pub use ontology::Ontology;
pub use query::EntityQuery;
pub use shape::{
    Cardinality, FieldId, FieldInfo, FilterCondition, FilterOp, GraphClaimId, Primitive,
    ShapeError, ShapeNode, SubjectId, TypeId, TypeInfo, TypeKind, ValueId,
};

/// Identifies one memory partition. A Pod's partition holds everything
/// derived from that Pod's content; the agent's own holds what is
/// genuinely Pod-independent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PartitionId {
    /// Everything learned from one Pod. Named by that Pod.
    Pod(String),
    /// The agent's own, Pod-independent memory.
    AgentPrivate,
}

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pod(id) => write!(f, "pod:{id}"),
            Self::AgentPrivate => write!(f, "agent-private"),
        }
    }
}

/// One entity, as a document. The `@type` and `@value_id` keys and the
/// nesting match what the Python API returns, so a document produced here
/// and one produced there describe the same thing the same way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub type_name: String,
    pub value_id: ValueId,
    pub fields: Map<String, Value>,
}

impl Entity {
    pub fn document(&self) -> Value {
        let mut out = Map::new();
        out.insert("@type".into(), Value::String(self.type_name.clone()));
        out.insert("@value_id".into(), Value::String(self.value_id.0.clone()));
        for (key, value) in &self.fields {
            out.insert(key.clone(), value.clone());
        }
        Value::Object(out)
    }
}

/// What can go wrong. Every variant names what the caller would need to
/// change, because a refusal that only says no is a refusal the agent
/// cannot act on and the person cannot debug.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphError {
    UnknownType(TypeId),
    UnknownTypeName {
        name: String,
        available: Vec<String>,
    },
    UnknownEntity {
        type_id: TypeId,
        value_id: ValueId,
    },
    /// The shape could not be walked, or a filter did not fit the field.
    Shape(ShapeError),
    /// A write did not conform to the type's shape.
    NonConformant {
        path: String,
        reason: String,
    },
    /// A write names something that is already there. Entities are
    /// claimed, not overwritten.
    AlreadyExists {
        kind: &'static str,
        id: String,
    },
    /// A type whose shape refers back to itself. The Python API refuses
    /// these too, and for the same reason: the shape tree would not
    /// terminate.
    RecursiveType(String),
    /// The storage underneath could not be reached or read. Says nothing
    /// about the query, which may be perfectly good.
    Unavailable(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType(id) => write!(f, "no entity type with id '{id}'"),
            Self::UnknownTypeName { name, available } => {
                write!(
                    f,
                    "no entity type named '{name}' — available: {}",
                    available.join(", ")
                )
            }
            Self::UnknownEntity { type_id, value_id } => {
                write!(
                    f,
                    "no entity of type '{type_id}' with value id '{value_id}'"
                )
            }
            Self::Shape(err) => write!(f, "{err}"),
            Self::NonConformant { path, reason } => {
                write!(f, "{path} does not conform to the type's shape: {reason}")
            }
            Self::AlreadyExists { kind, id } => write!(f, "{kind} already exists: {id}"),
            Self::RecursiveType(name) => write!(
                f,
                "type '{name}' refers to itself, which this shape model does not support"
            ),
            Self::Unavailable(reason) => write!(f, "the store could not be read: {reason}"),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<ShapeError> for GraphError {
    fn from(err: ShapeError) -> Self {
        Self::Shape(err)
    }
}

pub type GraphResult<T> = Result<T, GraphError>;

/// What a reader asks of the graph.
///
/// One handle is one partition. The operations are the ones the Python API
/// established as sufficient — discover types, load a shape, count, get
/// one, find many — each one call, with no fixed two-call sequence
/// required to get a useful result.
#[async_trait]
pub trait GraphReader: Send + Sync {
    /// Which partition this handle reaches. Informational: there is no
    /// operation that takes a different one.
    fn partition(&self) -> &PartitionId;

    /// Every entity type that has fields, in display order. The starting
    /// point for a caller that knows nothing about the ontology.
    async fn list_entity_types(&self) -> GraphResult<Vec<TypeInfo>>;

    async fn get_type(&self, type_id: &TypeId) -> GraphResult<TypeInfo>;

    /// Look a type up by its display name. Present because a caller
    /// working from natural language has a name and not an id; the name is
    /// still not an address, and two types may not share one.
    async fn get_type_by_name(&self, name: &str) -> GraphResult<TypeInfo>;

    /// The type's fields, and their fields, recursively — everything a
    /// query is built from.
    async fn get_shape(&self, type_id: &TypeId) -> GraphResult<ShapeNode>;

    /// How many entities match, without loading any of them. Cheap
    /// regardless of how wide the type is or how many match.
    async fn count(&self, query: &EntityQuery) -> GraphResult<usize>;

    /// The matching entities, up to the query's limit.
    async fn find(&self, query: &EntityQuery) -> GraphResult<Vec<Entity>>;

    /// One entity by its root value id.
    async fn get_entity(&self, type_id: &TypeId, value_id: &ValueId) -> GraphResult<Entity>;

    /// Start a query for a type, with its shape already loaded. The
    /// convenience the Python API spells `api(type_id)`.
    async fn query(&self, type_id: &TypeId) -> GraphResult<EntityQuery> {
        Ok(EntityQuery::new(self.get_shape(type_id).await?))
    }
}

/// The subject type a claim is about when nobody says otherwise.
pub const DEFAULT_SUBJECT_TYPE: &str = "mee:Subject:v1";

/// Who or what a claim is about, as read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectRef {
    pub id: String,
    pub name: String,
    /// The subject's type, where the registry holds it.
    pub type_id: String,
}

/// One node, as a handle returns it: where it is from — Pod, pod-tool,
/// member, time, device — what it is about where it is a claim, and its
/// value as a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    /// Unique within its pod-tool's zone.
    pub node_id: String,
    /// The subject, for a claim; `None` for a node about no subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<SubjectRef>,
    pub type_id: String,
    pub type_name: String,
    /// The root value's id, as the graph's callers address it:
    /// `<node id>/value`.
    pub root_value_id: String,
    pub value: Value,
    /// The member whose record it was made from.
    pub member: String,
    /// RFC 3339, UTC.
    pub at: String,
    /// The Pod it is in.
    pub pod_id: String,
    /// The pod-tool it was materialized from: the zone it is in.
    pub tool: String,
    /// The device its record was written on, where the record carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The members it is for, where it names them; empty for every member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readers: Vec<String>,
}

impl NodeView {
    /// The subject's id, for a claim.
    pub fn subject_id(&self) -> Option<&str> {
        self.subject.as_ref().map(|s| s.id.as_str())
    }

    pub fn is_claim(&self) -> bool {
        self.subject.is_some()
    }
}

/// Which way a relation is followed from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// To the nodes its relations lead to.
    Out,
    /// To the nodes whose relations lead to it.
    In,
}

/// One page of a long list, as a pod engine asks for it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PageRequest {
    /// One pod-tool's zone, or every zone the view reads.
    pub tool: Option<String>,
    pub type_id: Option<TypeId>,
    pub newest_first: bool,
    /// How many to pass over before the page.
    pub offset: usize,
    pub limit: usize,
}

/// A page, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub nodes: Vec<NodeView>,
    pub total: usize,
}

/// Someone or something a Pod's claims are about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubjectView {
    pub subject_id: String,
    pub display_name: String,
    pub type_id: Option<String>,
    /// The Pod whose claims are about it.
    pub pod_id: String,
    pub claims: usize,
    /// The names of the types claimed about them, in first-seen order.
    pub types: Vec<String>,
}

/// One pod-tool's zone of one Pod, as a reader is handed it: the ontology
/// it is typed by, and the nodes in it.
///
/// It reads and takes no writes. Which database is underneath and how many
/// Pods share it are the engine's.
#[async_trait]
pub trait PodGraph: GraphReader {
    /// Entity types with no fields: the ones a claim can be *about*
    /// (`Subject`, `Person`, …), as against the ones a claim carries.
    async fn subject_types(&self) -> GraphResult<Vec<TypeInfo>>;

    /// A type id if `named` is one, otherwise a type's display name. What a
    /// caller working from a person's words or a model's guess has.
    async fn resolve_type(&self, named: &str) -> GraphResult<TypeInfo> {
        match self.get_type(&TypeId::new(named)).await {
            Ok(info) => Ok(info),
            Err(_) => self.get_type_by_name(named).await,
        }
    }

    /// Nodes, oldest first: of `query`'s type and matching its filters when
    /// there is one, about one of `subjects` when given (claims alone), at
    /// most `query`'s limit.
    async fn nodes(
        &self,
        query: Option<&EntityQuery>,
        subjects: Option<&[String]>,
    ) -> GraphResult<Vec<NodeView>>;

    async fn node(&self, node_id: &str) -> GraphResult<Option<NodeView>>;

    async fn subjects(&self) -> GraphResult<Vec<SubjectView>>;

    /// The nodes in scope the node `node_id`'s relations named `relation`
    /// lead to, or that lead to it, oldest first. A relation out of the
    /// handle's zone leads to nothing.
    async fn related(
        &self,
        node_id: &str,
        relation: &str,
        direction: Direction,
    ) -> GraphResult<Vec<NodeView>>;
}

/// A type a view holds nodes of, with how many each Pod holds, by Pod id.
pub type PresentType = (TypeInfo, Vec<(String, usize)>);

/// Several Pods' structured data as one space to read: what a service that
/// answers across Pods queries, instead of asking each Pod in turn.
///
/// Every node and subject it returns carries the Pod it is in, so nothing
/// read through it loses where it came from. Which Pods it spans is fixed
/// when it is made (`ServiceHost::graph_view`), by the caller and not by
/// whatever drives the caller — a model asking a question cannot widen it.
/// It writes nothing: a node is made in one Pod, from the records of the
/// pod-tool that keeps it, in that Pod's record store.
#[async_trait]
pub trait GraphView: Send + Sync {
    /// The Pods this view spans, by id.
    fn pods(&self) -> &[String];

    /// Every type the view holds nodes of, with how many each Pod holds.
    async fn present_types(&self) -> GraphResult<Vec<PresentType>>;

    /// A type by id or by name.
    async fn resolve_type(&self, named: &str) -> GraphResult<TypeInfo>;

    async fn get_shape(&self, type_id: &TypeId) -> GraphResult<ShapeNode>;

    /// Nodes in any of the Pods, oldest first: of `query`'s type and
    /// matching its filters when there is one; about one of `subjects` when
    /// given (claims alone).
    async fn nodes(
        &self,
        query: Option<&EntityQuery>,
        subjects: Option<&[String]>,
    ) -> GraphResult<Vec<NodeView>>;

    /// A node by its id, in any zone the view reads.
    async fn node(&self, node_id: &str) -> GraphResult<Option<NodeView>>;

    /// Who and what the claims are about, each with its Pod.
    async fn subjects(&self) -> GraphResult<Vec<SubjectView>>;

    /// The nodes in scope that the node `node_id` of `tool`'s zone leads to
    /// by its relations named `relation`, or that lead to it, oldest first.
    async fn related(
        &self,
        tool: &str,
        node_id: &str,
        relation: &str,
        direction: Direction,
    ) -> GraphResult<Vec<NodeView>>;

    /// One page of the nodes, in order, and how many match in all: how a
    /// pod engine reads a long list without reading all of it.
    async fn page(&self, request: &PageRequest) -> GraphResult<Page>;

    /// `type_id` and every type it extends, nearest first: how a consumer
    /// reads a type it does not know as one it does.
    async fn generalizations(&self, type_id: &str) -> GraphResult<Vec<String>>;

    /// The tool ontologies of the zones this view was made for, each pod-tool
    /// once, and no others: what a consumer learns a pod-tool holds from.
    fn tool_ontologies(&self) -> Vec<materialize::ToolOntology>;
}
