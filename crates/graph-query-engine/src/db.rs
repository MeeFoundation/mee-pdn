//! The graph on `LadybugDB`: its schema, its catch-up and the queries behind the handles a
//! [`GraphCell`] makes. A zone is caught up a key at a time, a key's nodes and relations in one
//! transaction, and a zone new to the graph loads its backlog in bulk; reads wait while a catch-up
//! writes. Spec: the catch-up and queries capabilities of mia-docs' `graph-query-engine`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use lbug::{Connection, Database, LogicalType, PreparedStatement, SystemConfig, Value as Lv};
use serde_json::{Map, Number, Value};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::materialize::{by_key, Materialization, Materialized, Subject, ToolOntology};
use crate::ontology::RegistryTables;
use crate::query::EntityQuery;
use crate::record::{Record, RecordSource};
use crate::shape::{
    Cardinality, FieldId, FieldInfo, FilterCondition, FilterOp, Primitive, ShapeNode, TypeId,
    TypeInfo, TypeKind,
};
use crate::{
    Direction, Entity, GraphError, GraphReader, GraphResult, GraphView, NodeView, Ontology, Page,
    PageRequest, PartitionId, PodGraph, PresentType, SubjectRef, SubjectView, ValueId,
};

/// A zone new to a database bulk-loads a backlog of at least this many
/// nodes; a shorter one is applied key by key, which for a few nodes is
/// quicker than staging files.
pub const BULK_THRESHOLD: usize = 32;

/// Memory `LadybugDB` may use for its buffer pool. Its default is most of
/// the machine's RAM.
const BUFFER_POOL_BYTES: u64 = 512 * 1024 * 1024;
const THREADS: u64 = 4;

/// The address space a database reserves for its data. Left at its
/// default, `LadybugDB` reserves 8 TiB of virtual memory per database, and a
/// process holding a few dozen — a test run — runs out of address space.
/// Reserved, not used: 64 GiB is far beyond any Pod's nodes. On Android a
/// process has 512 GiB of address space (39-bit) and a 64 GiB mapping was
/// refused outright on a phone, so there it is 4 GiB.
#[cfg(not(target_os = "android"))]
const MAX_DB_BYTES: u64 = 1 << 36;
#[cfg(target_os = "android")]
const MAX_DB_BYTES: u64 = 1 << 32;

/// How deep a value tree is walked when it is read back. The registry's
/// deepest type is a few levels; this only stops a malformed graph.
const MAX_DEPTH: usize = 32;

/// Separates the members a node is for, and the parts of a link's key.
const SEP: char = '\u{1f}';

/// `index` is a keyword from `LadybugDB` 0.22 on, so the property is quoted.
const SCHEMA_DDL: &[&str] = &[
    "CREATE NODE TABLE TypeDef(type_id STRING, name STRING, canonical_name STRING, description STRING, kind STRING, primitive_representation STRING, PRIMARY KEY(type_id));",
    "CREATE NODE TABLE FieldSpec(field_id STRING, member_key STRING, display_name STRING, required BOOL, cardinality STRING, ordinal INT64, PRIMARY KEY(field_id));",
    "CREATE NODE TABLE Subject(subject_key STRING, pod_id STRING, subject_id STRING, display_name STRING, PRIMARY KEY(subject_key));",
    "CREATE NODE TABLE Node(node_key STRING, pod_id STRING, tool STRING, zone STRING, record_key STRING, node_id STRING, issued_by STRING, issued_at STRING, issued_on STRING, readers STRING, PRIMARY KEY(node_key));",
    "CREATE NODE TABLE ValueNode(value_id STRING, pod_id STRING, zone STRING, record_key STRING, representation_kind STRING, value UNION(bool_value BOOL, int_value INT64, float_value DOUBLE, decimal_value DECIMAL(38, 10), string_value STRING, date_value DATE, timestamp_value TIMESTAMP, blob_value BLOB, ref_subject_id STRING), PRIMARY KEY(value_id));",
    "CREATE NODE TABLE Link(link_key STRING, pod_id STRING, zone STRING, record_key STRING, from_key STRING, name STRING, to_key STRING, to_type STRING, PRIMARY KEY(link_key));",
    "CREATE REL TABLE TYPE_HAS_FIELD(FROM TypeDef TO FieldSpec);",
    "CREATE REL TABLE TYPE_EXTENDS(FROM TypeDef TO TypeDef);",
    "CREATE REL TABLE FIELD_HAS_FIELD(FROM FieldSpec TO FieldSpec);",
    "CREATE REL TABLE FIELD_VALUE_TYPE(FROM FieldSpec TO TypeDef);",
    "CREATE REL TABLE SUBJECT_HAS_TYPE(FROM Subject TO TypeDef);",
    "CREATE REL TABLE NODE_ABOUT(FROM Node TO Subject);",
    "CREATE REL TABLE NODE_HAS_VALUE(FROM Node TO ValueNode);",
    "CREATE REL TABLE VALUE_HAS_TYPE(FROM ValueNode TO TypeDef);",
    "CREATE REL TABLE VALUE_HAS_OBJECT_MEMBER(FROM ValueNode TO ValueNode, containment_id STRING, field_id STRING, member_key STRING, ordinal INT64);",
    "CREATE REL TABLE VALUE_HAS_LIST_ITEM(FROM ValueNode TO ValueNode, containment_id STRING, `index` INT64);",
];

/// A key qualified by its Pod. Length-prefixed, so no Pod id and id can
/// run together into another pair's key.
fn key(pod: &str, id: &str) -> String {
    format!("{}:{pod}/{id}", pod.len())
}

/// A pod-tool's zone in a Pod: the tag every node it materialized carries,
/// and what a query for that pod-tool's data is filtered by.
pub fn zone(pod: &str, tool: &str) -> String {
    key(pod, &format!("zone/{tool}"))
}

/// A node's key: its id, qualified by its Pod and its pod-tool.
fn node_key(pod: &str, tool: &str, id: &str) -> String {
    key(pod, &key(tool, id))
}

/// A node's root value, qualified. Its callers address it as
/// `<node id>/value`.
fn root_key(node_key: &str) -> String {
    format!("{node_key}/value")
}

/// UTC to the microsecond, `2026-11-21T10:00:00.000000Z`: one width, so
/// that `issued_at` orders as text the way it orders as time.
fn rfc3339(at: &OffsetDateTime) -> String {
    at.to_offset(UtcOffset::UTC)
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z"
        ))
        .unwrap_or_else(|_| at.to_string())
}

/// UTC with the fractional digits it needs, in threes: `2026-11-21T10:00:00Z`,
/// `2026-11-21T10:00:00.250Z`.
fn time_as_read(at: &OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    let nanos = at.nanosecond();
    let fraction = if nanos == 0 {
        String::new()
    } else if nanos.is_multiple_of(1_000_000) {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    };
    at.format(format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second]"
    ))
    .map_or_else(|_| at.to_string(), |whole| format!("{whole}{fraction}Z"))
}

// ---------------------------------------------------------------- records

/// One node on its way into the graph, with where it is from.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRow {
    pub node_id: String,
    pub type_id: String,
    pub value: Value,
    pub subject: Option<Subject>,
    pub readers: Vec<String>,
    pub member: String,
    /// RFC 3339, UTC, fixed width — so it orders as text.
    pub at: String,
    pub device: Option<String>,
}

/// One relation on its way into the graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinkRow {
    pub from: String,
    pub name: String,
    pub to_tool: String,
    pub to: String,
    /// The type its target is, or extends.
    pub to_type: String,
}

/// What one key holds in a graph.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KeyRows {
    pub nodes: Vec<NodeRow>,
    pub links: Vec<LinkRow>,
}

/// What catching up did, or what it has done in total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    /// Nodes that went in through the CSV bulk load.
    pub bulk: usize,
    /// Nodes applied key by key.
    pub appended: usize,
    /// Nodes and relations left out: a type the tool ontology does not
    /// declare or the registry does not hold, a value that does not fit its
    /// shape, a node id another key keeps, a relation not declared or from
    /// a node left out. Not tried again until their key's records change.
    pub skipped: usize,
    /// Keys whose records changed, applied again.
    pub replaced: usize,
    /// Keys no record is under any more, taken out.
    pub removed: usize,
    /// Nodes for other members alone, not built into this graph.
    pub withheld: usize,
}

impl Applied {
    fn add(&mut self, other: Applied) {
        self.bulk += other.bulk;
        self.appended += other.appended;
        self.skipped += other.skipped;
        self.replaced += other.replaced;
        self.removed += other.removed;
        self.withheld += other.withheld;
    }
}

/// One key as last applied.
struct KeyState {
    /// The records it was materialized from, by id and time.
    signature: String,
    /// What the materialization returned for them.
    output: Materialized,
    /// What the key holds in this graph.
    rows: KeyRows,
}

#[derive(Default)]
struct ZoneState {
    keys: HashMap<String, KeyState>,
    totals: Applied,
    /// The version of the zone's store when it was last caught up with,
    /// where the store gives one: a store at the same version is not read.
    version: Option<u64>,
    /// The materialization the zone was last caught up with
    /// ([`identity`]): one registered in its place makes every key of the
    /// zone materialized again, at the next catch-up.
    materialization: usize,
}

/// Which materialization this is: the same registration reads the same,
/// and one registered in its place reads differently.
fn identity(materialization: &dyn Materialization) -> usize {
    std::ptr::from_ref::<dyn Materialization>(materialization)
        .cast::<()>()
        .addr()
}

struct State {
    /// By [`zone`].
    zones: HashMap<String, ZoneState>,
    /// The Pods with a zone, each with its zones' pod-tools.
    pods: BTreeMap<String, Vec<String>>,
    shapes: HashMap<TypeId, ShapeNode>,
    generalizations: HashMap<String, Vec<String>>,
}

impl State {
    fn zone(&mut self, pod: &str, tool: &str) -> &mut ZoneState {
        let tools = self.pods.entry(pod.to_owned()).or_default();
        if !tools.iter().any(|t| t == tool) {
            tools.push(tool.to_owned());
        }
        self.zones.entry(zone(pod, tool)).or_default()
    }
}

/// One zone's records, as [`GraphDb::catch_up_many`] takes them: the Pod,
/// the pod-tool, its materialization, and every record of the zone.
pub type ZoneRecords<'a> = (&'a str, &'a str, &'a dyn Materialization, &'a [Record]);

/// The records under a key, by id and time: changed records, a changed
/// signature.
fn signature(records: &[Record]) -> String {
    records
        .iter()
        .map(|r| format!("{}@{}", r.id, rfc3339(&r.at)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What every key of one zone is worked out against: the pod-tool, its
/// tool ontology, and which key each node id belongs to.
struct ZoneContext<'a> {
    tool: &'a str,
    ontology: &'a ToolOntology,
    owner: &'a HashMap<String, String>,
}

/// One zone, worked out before anything is written.
struct Prepared {
    pod: String,
    tool: String,
    materialization: usize,
    /// Every key of the zone now: its signature, the materialization's
    /// output, what it holds in this graph, and what of it was skipped and
    /// withheld.
    keys: BTreeMap<String, (String, Materialized, KeyRows, usize, usize)>,
}

impl Prepared {
    fn nodes(&self) -> usize {
        self.keys
            .values()
            .map(|(_, _, rows, _, _)| rows.nodes.len())
            .sum()
    }
}

// ---------------------------------------------------------------- the database

/// One `LadybugDB`, holding the registry and every Pod an identity reads.
pub struct GraphDb {
    db: Database,
    known_types: HashSet<String>,
    bulk_threshold: usize,
    /// The identity whose graph this is: a node for some members alone is
    /// built only where this is one of them.
    identity: Option<String>,
    state: Mutex<State>,
    /// The table a bulk load fails at, set only by the crate's tests.
    bulk_fails_at: Option<&'static str>,
    /// Reads share it; a catch-up holds it alone, so no read sees part of
    /// what a catch-up writes, a bulk load's tables among them.
    gate: std::sync::RwLock<()>,
}

#[allow(clippy::needless_pass_by_value)] // the shape `map_err` hands an error over in
fn db_err(e: lbug::Error) -> String {
    e.to_string()
}

impl GraphDb {
    /// A database held in memory, its registry loaded from `ontology`, and
    /// no Pods yet.
    pub fn in_memory(ontology: &Ontology) -> Result<Self, String> {
        let config = SystemConfig::default()
            .buffer_pool_size(BUFFER_POOL_BYTES)
            .max_num_threads(THREADS)
            .max_db_size(MAX_DB_BYTES);
        let db = Database::in_memory(config).map_err(db_err)?;
        let tables = ontology.tables();
        let graph = Self {
            db,
            known_types: tables.types.iter().map(|t| t.type_id.clone()).collect(),
            bulk_threshold: BULK_THRESHOLD,
            identity: None,
            state: Mutex::new(State {
                zones: HashMap::new(),
                pods: BTreeMap::new(),
                shapes: HashMap::new(),
                generalizations: HashMap::new(),
            }),
            bulk_fails_at: None,
            gate: std::sync::RwLock::new(()),
        };
        {
            let conn = graph.conn()?;
            for ddl in SCHEMA_DDL {
                conn.query(ddl).map_err(db_err)?;
            }
            load_registry(&conn, tables)?;
        }
        Ok(graph)
    }

    /// Fail every bulk load at the `COPY` into `table`, after the tables
    /// before it have committed.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn with_bulk_failing_at(mut self, table: &'static str) -> Self {
        self.bulk_fails_at = Some(table);
        self
    }

    /// Bulk-load a new zone's backlog from this many nodes up, rather than
    /// [`BULK_THRESHOLD`].
    #[must_use]
    pub fn with_bulk_threshold(mut self, nodes: usize) -> Self {
        self.bulk_threshold = nodes.max(1);
        self
    }

    /// The graph of `identity`: nodes for some members alone are built here
    /// only where it is one of them. A graph of no identity builds none.
    #[must_use]
    pub fn of_identity(mut self, identity: Option<String>) -> Self {
        self.identity = identity;
        self
    }

    fn conn(&self) -> Result<Connection<'_>, String> {
        Connection::new(&self.db).map_err(db_err)
    }

    /// Everything applied to one Pod so far, every zone of it, by which path.
    pub fn totals(&self, pod: &str) -> Applied {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut total = Applied::default();
        for tool in state.pods.get(pod).into_iter().flatten() {
            if let Some(z) = state.zones.get(&zone(pod, tool)) {
                total.add(z.totals);
            }
        }
        total
    }

    /// The Pods this database holds nodes for.
    pub fn pods(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pods
            .keys()
            .cloned()
            .collect()
    }

    /// The pod-tools with a zone in `pod`.
    pub fn tools(&self, pod: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pods
            .get(pod)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `type_id` is `ancestor` or extends it.
    fn is_a(conn: &Connection<'_>, state: &mut State, type_id: &str, ancestor: &str) -> bool {
        if type_id == ancestor {
            return true;
        }
        if !state.generalizations.contains_key(type_id) {
            let found = generalizations(conn, type_id).unwrap_or_else(|_| vec![type_id.to_owned()]);
            state.generalizations.insert(type_id.to_owned(), found);
        }
        state
            .generalizations
            .get(type_id)
            .is_some_and(|general| general.iter().any(|t| t == ancestor))
    }

    /// Work out what every key of one zone holds in this graph, from its
    /// records, materializing only the keys whose records changed.
    fn prepare(
        &self,
        conn: &Connection<'_>,
        state: &mut State,
        pod: &str,
        tool: &str,
        materialization: &dyn Materialization,
        records: &[Record],
    ) -> Prepared {
        let ontology: ToolOntology = materialization.ontology();
        let groups = by_key(records);
        let mut outputs: BTreeMap<String, (String, Materialized)> = BTreeMap::new();
        {
            // What a replaced materialization made is not reused.
            let known = state
                .zones
                .get(&zone(pod, tool))
                .filter(|z| z.materialization == identity(materialization));
            for (k, recs) in &groups {
                let sig = signature(recs);
                let output = match known
                    .and_then(|z| z.keys.get(k))
                    .filter(|ks| ks.signature == sig)
                {
                    Some(ks) => ks.output.clone(),
                    None => materialization.materialize(k, recs),
                };
                outputs.insert(k.clone(), (sig, output));
            }
        }
        // A node id belongs to the first key, in byte order, that returns it.
        let mut owner: HashMap<String, String> = HashMap::new();
        for (k, (_, output)) in &outputs {
            for node in &output.nodes {
                owner.entry(node.id.clone()).or_insert_with(|| k.clone());
            }
        }
        let context = ZoneContext {
            tool,
            ontology: &ontology,
            owner: &owner,
        };
        let mut keys = BTreeMap::new();
        for (k, (sig, output)) in outputs {
            let recs = groups.get(&k).map(Vec::as_slice).unwrap_or_default();
            let (rows, skipped, withheld) = self.key_rows(conn, state, &context, &k, recs, &output);
            keys.insert(k, (sig, output, rows, skipped, withheld));
        }
        Prepared {
            pod: pod.to_owned(),
            tool: tool.to_owned(),
            materialization: identity(materialization),
            keys,
        }
    }

    /// What one key's output holds in this graph, and how many of its nodes
    /// and relations were skipped and withheld.
    fn key_rows(
        &self,
        conn: &Connection<'_>,
        state: &mut State,
        context: &ZoneContext<'_>,
        key: &str,
        recs: &[Record],
        output: &Materialized,
    ) -> (KeyRows, usize, usize) {
        let (mut rows, mut skipped, mut withheld) = (KeyRows::default(), 0, 0);
        let Some(newest) = recs.last() else {
            return (rows, skipped, withheld);
        };
        let mut present: HashMap<String, String> = HashMap::new();
        let mut held_back: HashSet<String> = HashSet::new();
        for node in &output.nodes {
            if context.owner.get(&node.id).map(String::as_str) != Some(key)
                || present.contains_key(&node.id)
                || held_back.contains(&node.id)
            {
                skipped += 1;
                continue;
            }
            if !context.ontology.declares_type(&node.type_id)
                || !self.known_types.contains(&node.type_id)
            {
                skipped += 1;
                continue;
            }
            if !node.readers.is_empty()
                && !self
                    .identity
                    .as_ref()
                    .is_some_and(|me| node.readers.contains(me))
            {
                withheld += 1;
                held_back.insert(node.id.clone());
                continue;
            }
            let source = node
                .record
                .as_ref()
                .and_then(|id| recs.iter().find(|r| &r.id == id))
                .unwrap_or(newest);
            present.insert(node.id.clone(), node.type_id.clone());
            rows.nodes.push(NodeRow {
                node_id: node.id.clone(),
                type_id: node.type_id.clone(),
                value: node.value.clone(),
                subject: node.subject.clone(),
                readers: node.readers.clone(),
                member: source.author.clone(),
                at: rfc3339(&source.at),
                device: source.device.clone(),
            });
        }
        for relation in &output.relations {
            if held_back.contains(&relation.from) {
                withheld += 1;
                continue;
            }
            let Some(declared) = context.ontology.relation(&relation.name) else {
                skipped += 1;
                continue;
            };
            let Some(from_type) = present.get(&relation.from) else {
                skipped += 1;
                continue;
            };
            if !Self::is_a(conn, state, from_type, &declared.from_type) {
                skipped += 1;
                continue;
            }
            let link = LinkRow {
                from: relation.from.clone(),
                name: relation.name.clone(),
                to_tool: relation
                    .to_tool
                    .clone()
                    .unwrap_or_else(|| context.tool.to_owned()),
                to: relation.to.clone(),
                to_type: declared.to_type.clone(),
            };
            if !rows.links.contains(&link) {
                rows.links.push(link);
            }
        }
        (rows, skipped, withheld)
    }

    /// Bring one zone up to date with its records ([`Self::catch_up_many`]).
    pub fn catch_up_zone(
        &self,
        pod: &str,
        tool: &str,
        materialization: &dyn Materialization,
        records: &[Record],
    ) -> Result<Applied, String> {
        self.catch_up_many(&[(pod, tool, materialization, records)])
    }

    /// Bring several zones up to date with their records. A key whose
    /// records changed has what it held replaced, in one transaction; a key
    /// no record is under any more holds nothing; a key whose records have
    /// not changed is not applied again. Zones new to the database load in
    /// bulk: on their own from [`BULK_THRESHOLD`] nodes, or together when
    /// there are at least two and that many nodes between them, since
    /// applying key by key commits each key by itself, which on a phone's
    /// storage takes some 50 ms a key.
    pub fn catch_up_many(&self, zones: &[ZoneRecords<'_>]) -> Result<Applied, String> {
        let _writing = self.gate.write().unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn()?;
        let prepared: Vec<Prepared> = zones
            .iter()
            .map(|(pod, tool, m, records)| self.prepare(&conn, &mut state, pod, tool, *m, records))
            .collect();

        let is_fresh = |state: &State, p: &Prepared| {
            state
                .zones
                .get(&zone(&p.pod, &p.tool))
                .is_none_or(|z| z.keys.is_empty())
        };
        let fresh: Vec<&Prepared> = prepared
            .iter()
            .filter(|p| is_fresh(&state, p) && p.nodes() > 0)
            .collect();
        let total: usize = fresh.iter().map(|p| p.nodes()).sum();
        let bulk: Vec<&Prepared> = if fresh.len() >= 2 && total >= self.bulk_threshold {
            fresh
        } else {
            fresh
                .into_iter()
                .filter(|p| p.nodes() >= self.bulk_threshold)
                .collect()
        };

        let mut applied = Applied::default();
        if !bulk.is_empty() {
            applied.add(self.load_in_bulk(&conn, &mut state, &bulk)?);
        }
        for p in &prepared {
            if !bulk.iter().any(|b| std::ptr::eq(*b, p)) {
                applied.add(self.apply_by_key(&conn, &mut state, p)?);
            }
        }
        for p in &prepared {
            state.zone(&p.pod, &p.tool).materialization = p.materialization;
        }
        Ok(applied)
    }

    /// Load fresh zones in one bulk load; where it fails, take out what it
    /// wrote, the subjects it made among them, and apply the zones key by key.
    fn load_in_bulk(
        &self,
        conn: &Connection<'_>,
        state: &mut State,
        which: &[&Prepared],
    ) -> Result<Applied, String> {
        let mut plans = Vec::new();
        let mut links = Vec::new();
        let mut per_zone = Vec::new();
        for p in which {
            let (mut planned, mut skipped, mut withheld) = (0, 0, 0);
            for (k, (_, _, rows, key_skipped, key_withheld)) in &p.keys {
                skipped += key_skipped;
                withheld += key_withheld;
                let (key_plans, failed) =
                    plan_key(conn, &mut state.shapes, &p.pod, &p.tool, k, rows);
                skipped += failed.len();
                planned += key_plans.len();
                plans.extend(key_plans);
                links.extend(link_rows(&p.pod, &p.tool, k, rows, &failed));
            }
            per_zone.push((planned, skipped, withheld));
        }
        let mut made_subjects = Vec::new();
        let loaded = bulk_load(
            conn,
            &plans,
            &links,
            &self.known_types,
            &mut made_subjects,
            self.bulk_fails_at,
        );
        let bulked = loaded.is_ok();
        if let Err(e) = loaded {
            let zones: Vec<String> = which.iter().map(|p| zone(&p.pod, &p.tool)).collect();
            tracing::warn!(
                ?zones,
                "a bulk load failed, its zones are applied key by key: {e}"
            );
            for p in which {
                clear_zone(conn, &p.pod, &p.tool)?;
            }
            remove_subjects(conn, &made_subjects)?;
        }
        let mut applied = Applied::default();
        for (p, (planned, skipped, withheld)) in which.iter().zip(per_zone) {
            let here = if bulked {
                Applied {
                    bulk: planned,
                    skipped,
                    withheld,
                    ..Applied::default()
                }
            } else {
                self.apply_by_key(conn, state, p)?
            };
            if bulked {
                let z = state.zone(&p.pod, &p.tool);
                for (k, (sig, output, rows, _, _)) in &p.keys {
                    z.keys.insert(
                        k.clone(),
                        KeyState {
                            signature: sig.clone(),
                            output: output.clone(),
                            rows: rows.clone(),
                        },
                    );
                }
                z.totals.add(here);
            }
            applied.add(here);
        }
        Ok(applied)
    }

    /// Apply every key of a zone whose rows changed, each in a transaction
    /// of its own, and take out every key no record is under any more.
    /// Every changed key is emptied before any is filled: a node id that
    /// moves to a key earlier in byte order would otherwise meet its old
    /// row under the key it leaves.
    fn apply_by_key(
        &self,
        conn: &Connection<'_>,
        state: &mut State,
        p: &Prepared,
    ) -> Result<Applied, String> {
        let mut applied = Applied::default();
        let gone: Vec<String> = {
            let z = state.zone(&p.pod, &p.tool);
            z.keys
                .keys()
                .filter(|k| !p.keys.contains_key(*k))
                .cloned()
                .collect()
        };
        for k in &gone {
            in_transaction(conn, |conn| remove_key(conn, &p.pod, &p.tool, k))?;
            state.zone(&p.pod, &p.tool).keys.remove(k);
            applied.removed += 1;
        }
        let mut changed = Vec::new();
        for (k, (sig, output, rows, skipped, withheld)) in &p.keys {
            let before = state
                .zones
                .get(&zone(&p.pod, &p.tool))
                .and_then(|z| z.keys.get(k));
            let had = before.is_some();
            if before.is_some_and(|ks| &ks.rows == rows) {
                // The same rows: nothing to apply. Keep the newer signature.
                if let Some(ks) = state.zone(&p.pod, &p.tool).keys.get_mut(k) {
                    ks.signature.clone_from(sig);
                    ks.output.clone_from(output);
                }
                continue;
            }
            if had {
                in_transaction(conn, |conn| remove_key(conn, &p.pod, &p.tool, k))?;
                // Forgotten until filled: a catch-up that stops before then
                // applies the key again.
                state.zone(&p.pod, &p.tool).keys.remove(k);
                applied.replaced += 1;
            }
            changed.push((k, sig, output, rows, skipped, withheld));
        }
        for (k, sig, output, rows, skipped, withheld) in changed {
            applied.skipped += skipped;
            applied.withheld += withheld;
            let (plans, failed) = plan_key(conn, &mut state.shapes, &p.pod, &p.tool, k, rows);
            applied.skipped += failed.len();
            let links = link_rows(&p.pod, &p.tool, k, rows, &failed);
            let refused = match in_transaction(conn, |conn| {
                append(conn, &plans, &links, &self.known_types)
            }) {
                Ok(()) => Refused::default(),
                Err(_) => append_each(conn, &plans, &links, &self.known_types),
            };
            applied.appended += plans.len().saturating_sub(refused.nodes);
            applied.skipped += refused.nodes + refused.links;
            state.zone(&p.pod, &p.tool).keys.insert(
                k.clone(),
                KeyState {
                    signature: sig.clone(),
                    output: output.clone(),
                    rows: rows.clone(),
                },
            );
        }
        state.zone(&p.pod, &p.tool).totals.add(applied);
        Ok(applied)
    }

    /// The store version a zone was last caught up with, where known.
    pub fn caught_up_at(&self, pod: &str, tool: &str) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .zones
            .get(&zone(pod, tool))
            .and_then(|z| z.version)
    }

    /// Whether a zone was last caught up with `materialization`.
    fn caught_up_with(&self, pod: &str, tool: &str, materialization: &dyn Materialization) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .zones
            .get(&zone(pod, tool))
            .is_some_and(|z| z.materialization == identity(materialization))
    }

    /// A zone was caught up with its store at `version`.
    pub fn caught_up(&self, pod: &str, tool: &str, version: Option<u64>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .zone(pod, tool)
            .version = version;
    }

    /// The nodes of `pods`, in `zones`, oldest first: of `query`'s type and
    /// matching its filters when there is one; about one of `subjects` when
    /// given; at most `query`'s limit.
    pub fn find_in(
        &self,
        pods: &[String],
        zones: &[String],
        query: Option<&EntityQuery>,
        subjects: Option<&[String]>,
    ) -> Result<Vec<NodeView>, String> {
        let scope = Scope {
            query,
            subjects,
            limit: query.map(EntityQuery::limit_value),
            ..Scope::new(pods, zones)
        };
        self.select(&scope)
    }

    /// One page of the nodes of `pods`, in `zones`, of `type_id` where
    /// given, newest first or oldest first; and how many there are in all.
    pub fn page(
        &self,
        pods: &[String],
        zones: &[String],
        type_id: Option<&TypeId>,
        newest_first: bool,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<NodeView>, usize), String> {
        let scope = Scope {
            type_id,
            newest_first,
            offset,
            limit: Some(limit),
            ..Scope::new(pods, zones)
        };
        let nodes = self.select(&scope)?;
        let total = self.count_scope(&Scope {
            type_id,
            ..Scope::new(pods, zones)
        })?;
        Ok((nodes, total))
    }

    pub fn get_in(
        &self,
        pods: &[String],
        zones: &[String],
        node_id: &str,
    ) -> Result<Option<NodeView>, String> {
        Ok(self
            .select(&Scope {
                node_id: Some(node_id),
                ..Scope::new(pods, zones)
            })?
            .into_iter()
            .next())
    }

    /// The node of `tool`'s zone whose root value is `root_value_id`
    /// (`<node id>/value`), if it is of `type_id`.
    pub fn get_by_root(
        &self,
        pod: &str,
        tool: &str,
        type_id: &TypeId,
        root_value_id: &str,
    ) -> Result<Option<NodeView>, String> {
        let root = key(pod, &key(tool, root_value_id));
        let (pods, zones) = ([pod.to_owned()], [zone(pod, tool)]);
        let scope = Scope {
            type_id: Some(type_id),
            root_value_id: Some(&root),
            ..Scope::new(&pods, &zones)
        };
        Ok(self.select(&scope)?.into_iter().next())
    }

    pub fn count_in(
        &self,
        pods: &[String],
        zones: &[String],
        query: &EntityQuery,
    ) -> Result<usize, String> {
        self.count_scope(&Scope {
            query: Some(query),
            ..Scope::new(pods, zones)
        })
    }

    fn count_scope(&self, scope: &Scope<'_>) -> Result<usize, String> {
        let _reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn()?;
        let (mut cypher, params) = scope.matching();
        cypher.push_str("RETURN count(DISTINCT root);");
        let rows = run(&conn, &cypher, params)?;
        Ok(usize::try_from(rows.first().and_then(|r| r.first()).map_or(0, int)).unwrap_or(0))
    }

    /// Who and what the claims of several Pods are about, in `zones`, each
    /// subject with its Pod: the same id in two Pods is two subjects. A node
    /// about no subject makes none.
    pub fn subjects_in(
        &self,
        pods: &[String],
        zones: &[String],
    ) -> Result<Vec<SubjectView>, String> {
        let _reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn()?;
        let rows = run(
            &conn,
            "MATCH (c:Node)-[:NODE_ABOUT]->(s:Subject)
             MATCH (c)-[:NODE_HAS_VALUE]->(:ValueNode)-[:VALUE_HAS_TYPE]->(t:TypeDef)
             WHERE c.pod_id IN $pods AND c.zone IN $zones
             OPTIONAL MATCH (s)-[:SUBJECT_HAS_TYPE]->(st:TypeDef)
             RETURN s.subject_id, s.display_name, st.type_id, t.name, c.pod_id
             ORDER BY c.issued_at, c.node_id;",
            vec![("pods", list(pods)), ("zones", list(zones))],
        )?;
        let mut by_id: BTreeMap<(String, String), SubjectView> = BTreeMap::new();
        for row in rows {
            let entry = by_id
                .entry((text(col(&row, 4)), text(col(&row, 0))))
                .or_insert_with(|| SubjectView {
                    subject_id: text(col(&row, 0)),
                    display_name: text(col(&row, 1)),
                    type_id: optional_text(col(&row, 2)),
                    pod_id: text(col(&row, 4)),
                    claims: 0,
                    types: Vec::new(),
                });
            entry.claims += 1;
            let type_name = text(col(&row, 3));
            if !entry.types.contains(&type_name) {
                entry.types.push(type_name);
            }
        }
        Ok(by_id.into_values().collect())
    }

    /// The types several Pods hold nodes of, in `zones`, with how many each
    /// Pod holds, by type name.
    pub fn present_types_in(
        &self,
        pods: &[String],
        zones: &[String],
    ) -> GraphResult<Vec<PresentType>> {
        let _reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        let rows = run(
            &conn,
            "MATCH (c:Node)-[:NODE_HAS_VALUE]->(:ValueNode)-[:VALUE_HAS_TYPE]->(t:TypeDef)
             WHERE c.pod_id IN $pods AND c.zone IN $zones
             RETURN t.type_id, t.name, t.canonical_name, t.description, t.kind, t.primitive_representation,
                    c.pod_id, count(c)
             ORDER BY t.name, c.pod_id;",
            vec![("pods", list(pods)), ("zones", list(zones))],
        )
        .map_err(GraphError::Unavailable)?;
        let mut out: Vec<PresentType> = Vec::new();
        for row in rows {
            let info = type_info(row.get(..6).unwrap_or_default());
            let held = (
                text(col(&row, 6)),
                usize::try_from(int(col(&row, 7))).unwrap_or(0),
            );
            match out.last_mut() {
                Some((last, counts)) if last.type_id == info.type_id => counts.push(held),
                _ => out.push((info, vec![held])),
            }
        }
        Ok(out)
    }

    /// The nodes in scope that the node `node_id` (of `tool`'s zone, where
    /// given) leads to by its relations named `relation`, or that lead to
    /// it, oldest first. A relation leads only to a node in scope, of the
    /// type it declares or one that extends it.
    pub fn related_in(
        &self,
        pods: &[String],
        zones: &[String],
        tool: Option<&str>,
        node_id: &str,
        relation: &str,
        direction: Direction,
    ) -> Result<Vec<NodeView>, String> {
        let reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn()?;
        let (here, there) = match direction {
            Direction::Out => ("from_key", "to_key"),
            Direction::In => ("to_key", "from_key"),
        };
        let tool_filter = if tool.is_some() {
            " AND c.tool = $tool"
        } else {
            ""
        };
        let mut params = vec![
            ("pods", list(pods)),
            ("zones", list(zones)),
            ("id", s(node_id)),
            ("name", s(relation)),
        ];
        if let Some(tool) = tool {
            params.push(("tool", s(tool)));
        }
        let rows = run(
            &conn,
            &format!(
                "MATCH (c:Node) WHERE c.pod_id IN $pods AND c.zone IN $zones AND c.node_id = $id{tool_filter}
                 MATCH (l:Link) WHERE l.{here} = c.node_key AND l.name = $name
                 MATCH (t:Node) WHERE t.node_key = l.{there} AND t.pod_id IN $pods AND t.zone IN $zones
                 MATCH (c)-[:NODE_HAS_VALUE]->(:ValueNode)-[:VALUE_HAS_TYPE]->(ct:TypeDef)
                 MATCH (t)-[:NODE_HAS_VALUE]->(:ValueNode)-[:VALUE_HAS_TYPE]->(tt:TypeDef)
                 RETURN DISTINCT t.node_key, ct.type_id, tt.type_id, l.to_type;"
            ),
            params,
        )?;
        let mut keys = Vec::new();
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            for row in rows {
                // The relation's target: the node followed to going out,
                // the node followed from coming in.
                let target_type = match direction {
                    Direction::Out => text(col(&row, 2)),
                    Direction::In => text(col(&row, 1)),
                };
                if Self::is_a(&conn, &mut state, &target_type, &text(col(&row, 3))) {
                    keys.push(text(col(&row, 0)));
                }
            }
        }
        drop(conn);
        drop(reading);
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        self.select(&Scope {
            node_keys: Some(&keys),
            ..Scope::new(pods, zones)
        })
    }

    /// `type_id` and every type it extends, nearest first, by the
    /// platform ontology's `extends`.
    pub fn generalizations(&self, type_id: &str) -> GraphResult<Vec<String>> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        generalizations(&conn, type_id).map_err(GraphError::Unavailable)
    }

    /// Every entity type that has fields, by name — `list_entity_types`.
    pub fn entity_types(&self) -> GraphResult<Vec<TypeInfo>> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        let rows = run(
            &conn,
            "MATCH (t:TypeDef)-[:TYPE_HAS_FIELD]->(:FieldSpec)
             WHERE t.kind = 'entity'
             RETURN DISTINCT t.type_id, t.name, t.canonical_name, t.description, t.kind, t.primitive_representation
             ORDER BY t.name;",
            no_params(),
        )
        .map_err(GraphError::Unavailable)?;
        Ok(rows.iter().map(|r| type_info(r)).collect())
    }

    /// Entity types with no fields — the ones a claim is about.
    pub fn subject_types(&self) -> GraphResult<Vec<TypeInfo>> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        let rows = run(
            &conn,
            "MATCH (t:TypeDef) WHERE t.kind = 'entity'
             OPTIONAL MATCH (t)-[:TYPE_HAS_FIELD]->(f:FieldSpec)
             WITH t, count(f) AS fields
             WHERE fields = 0
             RETURN t.type_id, t.name, t.canonical_name, t.description, t.kind, t.primitive_representation
             ORDER BY t.name;",
            no_params(),
        )
        .map_err(GraphError::Unavailable)?;
        Ok(rows.iter().map(|r| type_info(r)).collect())
    }

    pub fn get_type(&self, type_id: &TypeId) -> GraphResult<TypeInfo> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        get_type(&conn, type_id)
    }

    pub fn get_type_by_name(&self, name: &str) -> GraphResult<TypeInfo> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        let rows = run(
            &conn,
            "MATCH (t:TypeDef) WHERE t.name = $name
             RETURN t.type_id, t.name, t.canonical_name, t.description, t.kind, t.primitive_representation LIMIT 1;",
            vec![("name", s(name))],
        )
        .map_err(GraphError::Unavailable)?;
        match rows.first() {
            Some(row) => Ok(type_info(row)),
            None => Err(GraphError::UnknownTypeName {
                name: name.to_owned(),
                available: self.entity_types()?.into_iter().map(|t| t.name).collect(),
            }),
        }
    }

    /// The type's fields, and theirs, recursively — derived from the
    /// registry tables.
    pub fn shape(&self, type_id: &TypeId) -> GraphResult<ShapeNode> {
        let conn = self.conn().map_err(GraphError::Unavailable)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        shape_for(&conn, &mut state.shapes, type_id)
    }

    fn select(&self, scope: &Scope<'_>) -> Result<Vec<NodeView>, String> {
        if scope.subjects.is_some_and(<[String]>::is_empty) || scope.zones.is_empty() {
            return Ok(Vec::new());
        }
        let _reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
        let conn = self.conn()?;
        let (mut cypher, params) = scope.matching();
        cypher.push_str("WITH DISTINCT c, root, root_type, s\n");
        let order = if scope.newest_first {
            "ORDER BY c.issued_at DESC, c.node_id DESC"
        } else {
            "ORDER BY c.issued_at, c.node_id"
        };
        if scope.limit.is_some() || scope.offset > 0 {
            // In order, then the window: oldest first keeps the oldest.
            cypher.push_str(order);
            cypher.push('\n');
            if scope.offset > 0 {
                let _ = writeln!(cypher, "SKIP {}", scope.offset);
            }
            if let Some(limit) = scope.limit {
                let _ = writeln!(cypher, "LIMIT {}", limit.max(1));
            }
        }
        let _ = write!(
            cypher,
            "OPTIONAL MATCH (s)-[:SUBJECT_HAS_TYPE]->(st:TypeDef)
             RETURN c.node_id, c.issued_by, c.issued_at, s.subject_id, s.display_name, st.type_id,
                    root_type.type_id, root_type.name, root.value_id, c.pod_id, c.tool, c.issued_on, c.readers
             {order};"
        );
        let rows = run(&conn, &cypher, params)?;
        let roots: Vec<String> = rows.iter().map(|r| text(col(r, 8))).collect();
        let mut values = read_values(&conn, &roots)?;
        Ok(rows
            .iter()
            .map(|r| {
                let node_id = text(col(r, 0));
                let subject = optional_text(col(r, 3)).map(|id| SubjectRef {
                    id,
                    name: text(col(r, 4)),
                    type_id: optional_text(col(r, 5)).unwrap_or_default(),
                });
                NodeView {
                    root_value_id: format!("{node_id}/value"),
                    node_id,
                    subject,
                    member: text(col(r, 1)),
                    at: text(col(r, 2)),
                    type_id: text(col(r, 6)),
                    type_name: text(col(r, 7)),
                    value: values
                        .remove(&text(col(r, 8)))
                        .unwrap_or_else(|| Value::Object(Map::new())),
                    pod_id: text(col(r, 9)),
                    tool: text(col(r, 10)),
                    device: optional_text(col(r, 11)),
                    readers: optional_text(col(r, 12))
                        .map(|r| r.split(SEP).map(str::to_owned).collect())
                        .unwrap_or_default(),
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------- handles

/// Where one identity keeps its graph: made on first use, then shared by
/// every Pod it is in. Clone to share.
#[derive(Clone, Default)]
pub struct GraphCell {
    db: Arc<OnceLock<Result<Arc<GraphDb>, String>>>,
    identity: Option<String>,
}

/// One zone a handle reads: the Pod, the pod-tool, and the store its
/// records are in.
pub type ZoneSource = (String, String, Arc<dyn RecordSource>);

impl GraphCell {
    /// The graph of `identity`.
    pub fn of_identity(identity: impl Into<String>) -> Self {
        Self {
            db: Arc::default(),
            identity: Some(identity.into()),
        }
    }

    pub fn db(&self) -> GraphResult<Arc<GraphDb>> {
        self.db
            .get_or_init(|| {
                GraphDb::in_memory(&Ontology::platform())
                    .map(|g| Arc::new(g.of_identity(self.identity.clone())))
            })
            .clone()
            .map_err(|reason| {
                GraphError::Unavailable(format!("the graph database could not be opened: {reason}"))
            })
    }

    /// One pod-tool's zone of one Pod, kept up to date from `records` — the
    /// Pod's record store for that pod-tool.
    pub fn pod_graph(
        &self,
        pod: &str,
        kind: &str,
        records: Arc<dyn RecordSource>,
    ) -> GraphResult<Arc<dyn PodGraph>> {
        Ok(Arc::new(LadybugPodGraph {
            db: self.db()?,
            pod: pod.to_owned(),
            kind: kind.to_owned(),
            pods: vec![pod.to_owned()],
            zones: vec![zone(pod, kind)],
            partition: PartitionId::Pod(pod.to_owned()),
            records,
        }))
    }

    /// Zones of several Pods as one space to read, each kept up to date from
    /// its own store. A zone not given is not read, even where the database
    /// holds it.
    pub fn view(&self, zones: Vec<ZoneSource>) -> GraphResult<Arc<dyn GraphView>> {
        let mut ids: Vec<String> = Vec::new();
        for (pod, _, _) in &zones {
            if !ids.contains(pod) {
                ids.push(pod.clone());
            }
        }
        Ok(Arc::new(LadybugView {
            db: self.db()?,
            ids,
            zone_ids: zones.iter().map(|(pod, kind, _)| zone(pod, kind)).collect(),
            zones,
        }))
    }
}

/// Bring zones up to date with their stores, all at once: each is read
/// from its own store, none waits for another, and a zone whose store is at
/// the version it was last caught up with is not read.
async fn sync_zones(db: &GraphDb, zones: &[ZoneSource]) -> GraphResult<()> {
    let reads = zones.iter().map(|(pod, kind, records)| async move {
        let Some(materialization) = crate::materialize::materialization(kind) else {
            return Ok::<_, GraphError>(None);
        };
        let version = records.version().await.map_err(unavailable)?;
        if version.is_some()
            && db.caught_up_at(pod, kind) == version
            && db.caught_up_with(pod, kind, materialization.as_ref())
        {
            return Ok(None);
        }
        let records = records.records().await.map_err(unavailable)?;
        Ok(Some((
            pod.as_str(),
            kind.as_str(),
            materialization,
            records,
            version,
        )))
    });
    let mut read = Vec::new();
    for zone in futures::future::join_all(reads).await {
        if let Some(zone) = zone? {
            read.push(zone);
        }
    }
    let inputs: Vec<ZoneRecords<'_>> = read
        .iter()
        .map(|(pod, kind, m, records, _)| (*pod, *kind, m.as_ref(), records.as_slice()))
        .collect();
    db.catch_up_many(&inputs).map_err(unavailable)?;
    for (pod, kind, _, _, version) in &read {
        db.caught_up(pod, kind, *version);
    }
    Ok(())
}

/// Zones of several Pods, viewed in a shared database.
struct LadybugView {
    db: Arc<GraphDb>,
    ids: Vec<String>,
    zone_ids: Vec<String>,
    zones: Vec<ZoneSource>,
}

impl LadybugView {
    async fn sync(&self) -> GraphResult<()> {
        sync_zones(&self.db, &self.zones).await
    }
}

#[async_trait]
impl GraphView for LadybugView {
    fn pods(&self) -> &[String] {
        &self.ids
    }

    async fn present_types(&self) -> GraphResult<Vec<PresentType>> {
        self.sync().await?;
        self.db.present_types_in(&self.ids, &self.zone_ids)
    }

    async fn resolve_type(&self, named: &str) -> GraphResult<TypeInfo> {
        match self.db.get_type(&TypeId::new(named)) {
            Ok(info) => Ok(info),
            Err(_) => self.db.get_type_by_name(named),
        }
    }

    async fn get_shape(&self, type_id: &TypeId) -> GraphResult<ShapeNode> {
        self.db.shape(type_id)
    }

    async fn nodes(
        &self,
        query: Option<&EntityQuery>,
        subjects: Option<&[String]>,
    ) -> GraphResult<Vec<NodeView>> {
        self.sync().await?;
        self.db
            .find_in(&self.ids, &self.zone_ids, query, subjects)
            .map_err(unavailable)
    }

    async fn node(&self, node_id: &str) -> GraphResult<Option<NodeView>> {
        self.sync().await?;
        self.db
            .get_in(&self.ids, &self.zone_ids, node_id)
            .map_err(unavailable)
    }

    async fn subjects(&self) -> GraphResult<Vec<SubjectView>> {
        self.sync().await?;
        self.db
            .subjects_in(&self.ids, &self.zone_ids)
            .map_err(unavailable)
    }

    async fn related(
        &self,
        tool: &str,
        node_id: &str,
        relation: &str,
        direction: Direction,
    ) -> GraphResult<Vec<NodeView>> {
        self.sync().await?;
        self.db
            .related_in(
                &self.ids,
                &self.zone_ids,
                Some(tool),
                node_id,
                relation,
                direction,
            )
            .map_err(unavailable)
    }

    async fn page(&self, request: &PageRequest) -> GraphResult<Page> {
        self.sync().await?;
        let zones: Vec<String> = match &request.tool {
            Some(tool) => self
                .ids
                .iter()
                .map(|pod| zone(pod, tool))
                .filter(|z| self.zone_ids.contains(z))
                .collect(),
            None => self.zone_ids.clone(),
        };
        let (nodes, total) = self
            .db
            .page(
                &self.ids,
                &zones,
                request.type_id.as_ref(),
                request.newest_first,
                request.offset,
                request.limit,
            )
            .map_err(unavailable)?;
        Ok(Page { nodes, total })
    }

    async fn generalizations(&self, type_id: &str) -> GraphResult<Vec<String>> {
        self.db.generalizations(type_id)
    }

    fn tool_ontologies(&self) -> Vec<ToolOntology> {
        let mut kinds: Vec<&str> = Vec::new();
        for (_, kind, _) in &self.zones {
            if !kinds.contains(&kind.as_str()) {
                kinds.push(kind);
            }
        }
        kinds
            .into_iter()
            .filter_map(crate::materialize::materialization)
            .map(|m| m.ontology())
            .collect()
    }
}

/// One graph per identity, for a backend that serves several in one
/// process — the local world, a node. Each identity has one database for
/// all of its Pods, and no identity shares another's.
#[derive(Clone, Default)]
pub struct GraphCells(Arc<Mutex<HashMap<String, GraphCell>>>);

impl GraphCells {
    pub fn of(&self, who: &str) -> GraphCell {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(who.to_owned())
            .or_insert_with(|| GraphCell::of_identity(who))
            .clone()
    }
}

/// One pod-tool's zone of one Pod, in a shared database.
struct LadybugPodGraph {
    db: Arc<GraphDb>,
    pod: String,
    kind: String,
    pods: Vec<String>,
    zones: Vec<String>,
    partition: PartitionId,
    records: Arc<dyn RecordSource>,
}

fn unavailable(reason: String) -> GraphError {
    GraphError::Unavailable(reason)
}

impl LadybugPodGraph {
    /// Bring this zone up to date with its store.
    async fn sync(&self) -> GraphResult<()> {
        sync_zones(
            &self.db,
            &[(
                self.pod.clone(),
                self.kind.clone(),
                Arc::clone(&self.records),
            )],
        )
        .await
    }

    fn entity(view: NodeView) -> Entity {
        let mut fields = view.value.as_object().cloned().unwrap_or_default();
        if let Some(subject) = &view.subject {
            fields.insert("@subject_id".into(), Value::String(subject.id.clone()));
        }
        fields.insert("@node_id".into(), Value::String(view.node_id));
        Entity {
            type_name: view.type_name,
            value_id: ValueId::new(view.root_value_id),
            fields,
        }
    }
}

#[async_trait]
impl GraphReader for LadybugPodGraph {
    fn partition(&self) -> &PartitionId {
        &self.partition
    }

    async fn list_entity_types(&self) -> GraphResult<Vec<TypeInfo>> {
        self.db.entity_types()
    }

    async fn get_type(&self, type_id: &TypeId) -> GraphResult<TypeInfo> {
        self.db.get_type(type_id)
    }

    async fn get_type_by_name(&self, name: &str) -> GraphResult<TypeInfo> {
        self.db.get_type_by_name(name)
    }

    async fn get_shape(&self, type_id: &TypeId) -> GraphResult<ShapeNode> {
        self.db.shape(type_id)
    }

    async fn count(&self, query: &EntityQuery) -> GraphResult<usize> {
        self.sync().await?;
        self.db
            .count_in(&self.pods, &self.zones, query)
            .map_err(unavailable)
    }

    async fn find(&self, query: &EntityQuery) -> GraphResult<Vec<Entity>> {
        Ok(self
            .nodes(Some(query), None)
            .await?
            .into_iter()
            .map(Self::entity)
            .collect())
    }

    async fn get_entity(&self, type_id: &TypeId, value_id: &ValueId) -> GraphResult<Entity> {
        self.sync().await?;
        self.db
            .get_by_root(&self.pod, &self.kind, type_id, &value_id.0)
            .map_err(unavailable)?
            .map(Self::entity)
            .ok_or_else(|| GraphError::UnknownEntity {
                type_id: type_id.clone(),
                value_id: value_id.clone(),
            })
    }
}

#[async_trait]
impl PodGraph for LadybugPodGraph {
    async fn subject_types(&self) -> GraphResult<Vec<TypeInfo>> {
        self.db.subject_types()
    }

    async fn nodes(
        &self,
        query: Option<&EntityQuery>,
        subjects: Option<&[String]>,
    ) -> GraphResult<Vec<NodeView>> {
        self.sync().await?;
        self.db
            .find_in(&self.pods, &self.zones, query, subjects)
            .map_err(unavailable)
    }

    async fn node(&self, node_id: &str) -> GraphResult<Option<NodeView>> {
        self.sync().await?;
        self.db
            .get_in(&self.pods, &self.zones, node_id)
            .map_err(unavailable)
    }

    async fn subjects(&self) -> GraphResult<Vec<SubjectView>> {
        self.sync().await?;
        self.db
            .subjects_in(&self.pods, &self.zones)
            .map_err(unavailable)
    }

    async fn related(
        &self,
        node_id: &str,
        relation: &str,
        direction: Direction,
    ) -> GraphResult<Vec<NodeView>> {
        self.sync().await?;
        self.db
            .related_in(
                &self.pods,
                &self.zones,
                Some(&self.kind),
                node_id,
                relation,
                direction,
            )
            .map_err(unavailable)
    }
}

// ---------------------------------------------------------------- reading

/// What a select is restricted to: always its Pods and zones.
struct Scope<'a> {
    pods: &'a [String],
    zones: &'a [String],
    query: Option<&'a EntityQuery>,
    type_id: Option<&'a TypeId>,
    subjects: Option<&'a [String]>,
    node_id: Option<&'a str>,
    /// Qualified.
    node_keys: Option<&'a [String]>,
    /// Qualified.
    root_value_id: Option<&'a str>,
    limit: Option<usize>,
    /// Newest first, rather than oldest: for a page of a long list.
    newest_first: bool,
    /// How many of the matching nodes to pass over, in order, before the
    /// limit.
    offset: usize,
}

impl<'a> Scope<'a> {
    fn new(pods: &'a [String], zones: &'a [String]) -> Self {
        Self {
            pods,
            zones,
            query: None,
            type_id: None,
            subjects: None,
            node_id: None,
            node_keys: None,
            root_value_id: None,
            limit: None,
            newest_first: false,
            offset: 0,
        }
    }

    /// The `MATCH … WHERE …` that finds the nodes in scope, binding `c`,
    /// `root`, `root_type` and `s` (null for a node about no subject), and
    /// its parameters.
    fn matching(&self) -> (String, Vec<(String, Lv)>) {
        let mut lines = vec![
            "MATCH (c:Node)-[:NODE_HAS_VALUE]->(root:ValueNode)".to_owned(),
            "MATCH (root)-[:VALUE_HAS_TYPE]->(root_type:TypeDef)".to_owned(),
            "WHERE c.pod_id IN $pods AND c.zone IN $zones".to_owned(),
        ];
        let mut params: Vec<(String, Lv)> = vec![
            ("pods".into(), list(self.pods)),
            ("zones".into(), list(self.zones)),
        ];
        if let Some(type_id) = self.query.map(EntityQuery::type_id).or(self.type_id) {
            lines.push("  AND root_type.type_id = $type_id".into());
            params.push(("type_id".into(), s(&type_id.0)));
        }
        if let Some(node_id) = self.node_id {
            lines.push("  AND c.node_id = $node_id".into());
            params.push(("node_id".into(), s(node_id)));
        }
        if let Some(keys) = self.node_keys {
            lines.push("  AND c.node_key IN $node_keys".into());
            params.push(("node_keys".into(), list(keys)));
        }
        if let Some(root) = self.root_value_id {
            lines.push("  AND root.value_id = $root_value_id".into());
            params.push(("root_value_id".into(), s(root)));
        }
        for (index, condition) in self
            .query
            .map(EntityQuery::filters)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            append_filter(&mut lines, &mut params, condition, index);
        }
        match self.subjects {
            Some(subjects) => {
                lines.push(
                    "MATCH (c)-[:NODE_ABOUT]->(s:Subject) WHERE s.subject_id IN $subjects".into(),
                );
                params.push(("subjects".into(), list(subjects)));
            }
            None => lines.push("OPTIONAL MATCH (c)-[:NODE_ABOUT]->(s:Subject)".into()),
        }
        let mut cypher = lines.join("\n");
        cypher.push('\n');
        (cypher, params)
    }
}

/// `type_id` and every type it extends, nearest first.
fn generalizations(conn: &Connection<'_>, type_id: &str) -> Result<Vec<String>, String> {
    let mut out = vec![type_id.to_owned()];
    while out.len() < MAX_DEPTH {
        let Some(nearest) = out.last() else { break };
        let rows = run(
            conn,
            "MATCH (a:TypeDef)-[:TYPE_EXTENDS]->(b:TypeDef) WHERE a.type_id = $type_id RETURN b.type_id;",
            vec![("type_id", s(nearest))],
        )?;
        match rows.first().map(|r| text(col(r, 0))) {
            Some(general) if !out.contains(&general) => out.push(general),
            _ => break,
        }
    }
    Ok(out)
}

/// One filter, as `append_filter_condition` writes it: walk the path edge
/// by edge, matching each edge by its field id, then compare the leaf.
fn append_filter(
    lines: &mut Vec<String>,
    params: &mut Vec<(String, Lv)>,
    condition: &FilterCondition,
    index: usize,
) {
    let mut parent = "root".to_owned();
    let mut leaf_has_where = false;
    for (depth, field) in condition.path.iter().enumerate() {
        let prefix = format!("filter_{index}_{depth}");
        let edge = format!("{prefix}_edge");
        let field_param = format!("{prefix}_field");
        let is_leaf = depth == condition.path.len() - 1;
        params.push((field_param.clone(), s(&field.field_id.0)));
        if field.cardinality == Cardinality::Many {
            let list = format!("{prefix}_list");
            let item = format!("{prefix}_item");
            lines.push(format!(
                "MATCH ({parent})-[{edge}:VALUE_HAS_OBJECT_MEMBER]->({list}:ValueNode)"
            ));
            lines.push(format!("  WHERE {edge}.field_id = ${field_param}"));
            lines.push(format!(
                "MATCH ({list})-[:VALUE_HAS_LIST_ITEM]->({item}:ValueNode)"
            ));
            parent = item;
            if is_leaf {
                leaf_has_where = false;
            }
        } else {
            let member = format!("{prefix}_value");
            lines.push(format!(
                "MATCH ({parent})-[{edge}:VALUE_HAS_OBJECT_MEMBER]->({member}:ValueNode)"
            ));
            lines.push(format!("  WHERE {edge}.field_id = ${field_param}"));
            parent = member;
            if is_leaf {
                leaf_has_where = true;
            }
        }
    }
    let Some(leaf) = condition.path.last() else {
        return;
    };
    let value_param = format!("filter_{index}_value");
    let (arm, bound) = filter_operand(leaf, &value_param);
    let operator = match condition.op {
        FilterOp::Eq | FilterOp::Contains => "=",
        FilterOp::Gt => ">",
        FilterOp::Gte => ">=",
        FilterOp::Lt => "<",
        FilterOp::Lte => "<=",
    };
    let keyword = if leaf_has_where { "  AND" } else { "WHERE" };
    lines.push(format!(
        "{keyword} union_extract({parent}.value, '{arm}') {operator} {bound}"
    ));
    params.push((value_param, filter_value(leaf, &condition.value)));
}

/// The union arm a leaf's values live in, and how the bound value is
/// written so it compares as that type.
fn filter_operand(field: &FieldInfo, param: &str) -> (&'static str, String) {
    if field.is_reference() {
        return ("ref_subject_id", format!("${param}"));
    }
    match field.value_type.primitive {
        Primitive::Bool => ("bool_value", format!("${param}")),
        Primitive::Int => ("int_value", format!("${param}")),
        Primitive::Float => ("float_value", format!("${param}")),
        Primitive::Decimal => (
            "decimal_value",
            format!("CAST(${param} AS DECIMAL(38, 10))"),
        ),
        Primitive::Date => ("date_value", format!("date(${param})")),
        Primitive::Timestamp => ("timestamp_value", format!("timestamp(${param})")),
        Primitive::String | Primitive::Reference => ("string_value", format!("${param}")),
    }
}

/// A filter's value as a parameter. The query builder has already checked
/// it against the field's type.
fn filter_value(field: &FieldInfo, value: &Value) -> Lv {
    if field.is_reference() {
        return Lv::String(scalar_text(value));
    }
    match field.value_type.primitive {
        Primitive::Bool => Lv::Bool(value.as_bool().unwrap_or_default()),
        Primitive::Int => Lv::Int64(value.as_i64().unwrap_or_default()),
        Primitive::Float => Lv::Double(value.as_f64().unwrap_or_default()),
        _ => Lv::String(scalar_text(value)),
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One node of a value tree, as read back.
struct ReadNode {
    kind: String,
    payload: Option<Value>,
    /// Object members with their ordinal and key; list items by index.
    members: Vec<(i64, String, String)>,
    items: Vec<(i64, String)>,
}

impl ReadNode {
    fn new(kind: String, payload: Option<Value>) -> Self {
        Self {
            kind,
            payload,
            members: Vec::new(),
            items: Vec::new(),
        }
    }
}

/// The values under `roots`, as documents, walked a level at a time.
fn read_values(conn: &Connection<'_>, roots: &[String]) -> Result<HashMap<String, Value>, String> {
    let mut nodes: HashMap<String, ReadNode> = roots
        .iter()
        .map(|r| (r.clone(), ReadNode::new("object".into(), None)))
        .collect();
    let mut level: Vec<String> = roots.to_vec();
    for _ in 0..MAX_DEPTH {
        if level.is_empty() {
            break;
        }
        let ids = Lv::List(LogicalType::String, level.iter().map(|x| s(x)).collect());
        let mut next = Vec::new();
        let members = run(
            conn,
            "MATCH (p:ValueNode)-[e:VALUE_HAS_OBJECT_MEMBER]->(c:ValueNode) WHERE p.value_id IN $ids
             RETURN p.value_id, e.ordinal, e.member_key, c.value_id, c.representation_kind, c.value;",
            vec![("ids", ids.clone())],
        )?;
        for row in members {
            let (parent, child) = (text(col(&row, 0)), text(col(&row, 3)));
            if let Some(node) = nodes.get_mut(&parent) {
                node.members
                    .push((int(col(&row, 1)), text(col(&row, 2)), child.clone()));
            }
            next.push(child.clone());
            nodes.insert(
                child,
                ReadNode::new(text(col(&row, 4)), payload(col(&row, 5))),
            );
        }
        let items = run(
            conn,
            "MATCH (p:ValueNode)-[e:VALUE_HAS_LIST_ITEM]->(c:ValueNode) WHERE p.value_id IN $ids
             RETURN p.value_id, e.`index`, c.value_id, c.representation_kind, c.value;",
            vec![("ids", ids)],
        )?;
        for row in items {
            let (parent, child) = (text(col(&row, 0)), text(col(&row, 2)));
            if let Some(node) = nodes.get_mut(&parent) {
                node.items.push((int(col(&row, 1)), child.clone()));
            }
            next.push(child.clone());
            nodes.insert(
                child,
                ReadNode::new(text(col(&row, 3)), payload(col(&row, 4))),
            );
        }
        next.retain(|id| {
            nodes
                .get(id)
                .is_some_and(|n| n.kind == "object" || n.kind == "list")
        });
        level = next;
    }
    Ok(roots
        .iter()
        .map(|r| (r.clone(), document(&nodes, r, 0)))
        .collect())
}

fn document(nodes: &HashMap<String, ReadNode>, id: &str, depth: usize) -> Value {
    let Some(node) = nodes.get(id) else {
        return Value::Null;
    };
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    match node.kind.as_str() {
        "object" => {
            let mut members = node.members.clone();
            members.sort();
            let mut out = Map::new();
            for (_, key, child) in members {
                out.insert(key, document(nodes, &child, depth + 1));
            }
            Value::Object(out)
        }
        "list" => {
            let mut items = node.items.clone();
            items.sort();
            Value::Array(
                items
                    .iter()
                    .map(|(_, child)| document(nodes, child, depth + 1))
                    .collect(),
            )
        }
        _ => node.payload.clone().unwrap_or(Value::Null),
    }
}

/// A value node's payload as JSON.
fn payload(value: &Lv) -> Option<Value> {
    let inner = match value {
        Lv::Union { value, .. } => value.as_ref(),
        other => other,
    };
    Some(match inner {
        Lv::Null(_) => return None,
        Lv::String(s) => Value::String(s.clone()),
        Lv::Bool(b) => Value::Bool(*b),
        Lv::Int64(i) => Value::Number((*i).into()),
        Lv::Int32(i) => Value::Number((*i).into()),
        Lv::Double(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Lv::Decimal(d) => {
            let text = d.normalize().to_string();
            text.parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::String(text))
        }
        Lv::Date(d) => Value::String(d.to_string()),
        Lv::Timestamp(t) => Value::String(time_as_read(t)),
        other => Value::String(other.to_string()),
    })
}

/// Read where a column is missing: no value.
static NULL: LazyLock<Lv> = LazyLock::new(|| Lv::Null(LogicalType::Any));

/// Column `i` of a result row, or no value where the row has none.
fn col(row: &[Lv], i: usize) -> &Lv {
    row.get(i).unwrap_or(&NULL)
}

fn text(value: &Lv) -> String {
    optional_text(value).unwrap_or_default()
}

fn optional_text(value: &Lv) -> Option<String> {
    match value {
        Lv::Null(_) => None,
        Lv::String(s) if s.is_empty() => None,
        Lv::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn int(value: &Lv) -> i64 {
    match value {
        Lv::Int64(i) => *i,
        Lv::Int32(i) => (*i).into(),
        Lv::UInt64(i) => i64::try_from(*i).unwrap_or(i64::MAX),
        _ => 0,
    }
}

fn boolean(value: &Lv) -> bool {
    matches!(value, Lv::Bool(true))
}

fn list(pods: &[String]) -> Lv {
    Lv::List(LogicalType::String, pods.iter().map(|p| s(p)).collect())
}

fn s(value: &str) -> Lv {
    Lv::String(value.to_owned())
}

fn no_params() -> Vec<(&'static str, Lv)> {
    Vec::new()
}

fn run(
    conn: &Connection<'_>,
    cypher: &str,
    params: Vec<(impl AsRef<str>, Lv)>,
) -> Result<Vec<Vec<Lv>>, String> {
    let mut statement = conn
        .prepare(cypher)
        .map_err(|e| format!("{e} — in: {cypher}"))?;
    let params: Vec<(String, Lv)> = params
        .into_iter()
        .map(|(k, v)| (k.as_ref().to_owned(), v))
        .collect();
    let borrowed: Vec<(&str, Lv)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    let result = conn.execute(&mut statement, borrowed).map_err(db_err)?;
    Ok(result.collect())
}

// ---------------------------------------------------------------- the registry

fn type_info(row: &[Lv]) -> TypeInfo {
    TypeInfo {
        type_id: TypeId::new(text(col(row, 0))),
        name: text(col(row, 1)),
        canonical_name: optional_text(col(row, 2)),
        description: optional_text(col(row, 3)),
        kind: TypeKind::parse(&text(col(row, 4))),
        primitive: Primitive::parse(optional_text(col(row, 5)).as_deref()),
    }
}

fn get_type(conn: &Connection<'_>, type_id: &TypeId) -> GraphResult<TypeInfo> {
    let rows = run(
        conn,
        "MATCH (t:TypeDef) WHERE t.type_id = $id
         RETURN t.type_id, t.name, t.canonical_name, t.description, t.kind, t.primitive_representation LIMIT 1;",
        vec![("id", s(&type_id.0))],
    )
    .map_err(GraphError::Unavailable)?;
    rows.first()
        .map(|r| type_info(r))
        .ok_or_else(|| GraphError::UnknownType(type_id.clone()))
}

fn shape_for(
    conn: &Connection<'_>,
    cache: &mut HashMap<TypeId, ShapeNode>,
    type_id: &TypeId,
) -> GraphResult<ShapeNode> {
    if let Some(shape) = cache.get(type_id) {
        return Ok(shape.clone());
    }
    let shape = build_shape(conn, get_type(conn, type_id)?, &HashSet::new())?;
    cache.insert(type_id.clone(), shape.clone());
    Ok(shape)
}

fn build_shape(
    conn: &Connection<'_>,
    info: TypeInfo,
    seen: &HashSet<TypeId>,
) -> GraphResult<ShapeNode> {
    if seen.contains(&info.type_id) {
        return Err(GraphError::RecursiveType(info.name));
    }
    let mut next_seen = seen.clone();
    next_seen.insert(info.type_id.clone());
    let rows = run(
        conn,
        "MATCH (t:TypeDef)-[:TYPE_HAS_FIELD]->(f:FieldSpec)-[:FIELD_VALUE_TYPE]->(vt:TypeDef)
         WHERE t.type_id = $id
         RETURN f.field_id, f.member_key, f.display_name, f.required, f.cardinality, f.ordinal,
                vt.type_id, vt.name, vt.canonical_name, vt.description, vt.kind, vt.primitive_representation
         ORDER BY f.ordinal;",
        vec![("id", s(&info.type_id.0))],
    )
    .map_err(GraphError::Unavailable)?;
    let mut fields = Vec::with_capacity(rows.len());
    for row in rows {
        let value_type = type_info(row.get(6..).unwrap_or_default());
        let child_shape =
            if value_type.kind == TypeKind::Entity && has_fields(conn, &value_type.type_id)? {
                Some(Box::new(build_shape(conn, value_type.clone(), &next_seen)?))
            } else {
                None
            };
        fields.push(FieldInfo {
            field_id: FieldId::new(text(col(&row, 0))),
            member_key: text(col(&row, 1)),
            display_name: text(col(&row, 2)),
            required: boolean(col(&row, 3)),
            cardinality: Cardinality::parse(&text(col(&row, 4))),
            ordinal: int(col(&row, 5)),
            value_type,
            child_shape,
        });
    }
    Ok(ShapeNode::new(info, fields))
}

fn has_fields(conn: &Connection<'_>, type_id: &TypeId) -> GraphResult<bool> {
    let rows = run(
        conn,
        "MATCH (t:TypeDef)-[:TYPE_HAS_FIELD]->(:FieldSpec) WHERE t.type_id = $id RETURN count(t);",
        vec![("id", s(&type_id.0))],
    )
    .map_err(GraphError::Unavailable)?;
    Ok(rows.first().and_then(|r| r.first()).map_or(0, int) > 0)
}

/// The registry goes in the way `ingestion.load_registry` puts it in:
/// written out as CSV and `COPY`d, once, into a fresh database.
#[allow(clippy::too_many_lines)] // the registry's five tables, each copied in turn
fn load_registry(conn: &Connection<'_>, tables: &RegistryTables) -> Result<(), String> {
    let dir = Staging::new()?;
    let types: Vec<Vec<String>> = tables
        .types
        .iter()
        .map(|t| {
            vec![
                t.type_id.clone(),
                t.name.clone(),
                t.canonical_name.clone().unwrap_or_default(),
                t.description.clone().unwrap_or_default(),
                t.kind.clone(),
                t.primitive_representation.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let fields: Vec<Vec<String>> = tables
        .fields
        .iter()
        .map(|f| {
            vec![
                f.field_id.clone(),
                f.member_key.clone(),
                f.display_name.clone(),
                f.required.to_string(),
                f.cardinality.clone(),
                f.ordinal.to_string(),
            ]
        })
        .collect();
    let pairs = |rows: &[(String, String)]| -> Vec<Vec<String>> {
        rows.iter()
            .map(|(a, b)| vec![a.clone(), b.clone()])
            .collect()
    };
    copy(
        conn,
        &dir,
        "TypeDef",
        &[
            "type_id",
            "name",
            "canonical_name",
            "description",
            "kind",
            "primitive_representation",
        ],
        &types,
    )?;
    copy(
        conn,
        &dir,
        "FieldSpec",
        &[
            "field_id",
            "member_key",
            "display_name",
            "required",
            "cardinality",
            "ordinal",
        ],
        &fields,
    )?;
    copy(
        conn,
        &dir,
        "TYPE_HAS_FIELD",
        &["from", "to"],
        &pairs(&tables.type_has_field),
    )?;
    copy(
        conn,
        &dir,
        "FIELD_VALUE_TYPE",
        &["from", "to"],
        &pairs(&tables.field_value_type),
    )?;
    copy(
        conn,
        &dir,
        "TYPE_EXTENDS",
        &["from", "to"],
        &pairs(&tables.extends),
    )?;
    Ok(())
}

// ---------------------------------------------------------------- writing

/// A scalar's payload, by the union arm it goes into.
#[derive(Debug, Clone)]
enum Payload {
    None,
    String(String),
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(String),
    Date(String),
    Timestamp(String),
    Reference(String),
}

struct ValueRow {
    value_id: String,
    kind: &'static str,
    payload: Payload,
}

struct MemberRow {
    from: String,
    to: String,
    field_id: String,
    member_key: String,
    ordinal: i64,
}

struct ItemRow {
    from: String,
    to: String,
    index: i64,
}

/// Everything one node becomes, with every key qualified by its Pod and
/// zone.
struct Plan<'a> {
    pod: &'a str,
    tool: &'a str,
    zone: String,
    record_key: &'a str,
    node: &'a NodeRow,
    node_key: String,
    root: String,
    values: Vec<ValueRow>,
    members: Vec<MemberRow>,
    items: Vec<ItemRow>,
}

/// A child's id is its parent's, the member key and the ordinal — so ids
/// are stable, and the same node always makes the same graph.
fn value_id(parent: &str, member_key: &str, ordinal: i64) -> String {
    format!("{parent}/{member_key}/{ordinal}")
}

impl<'a> Plan<'a> {
    fn of(
        pod: &'a str,
        tool: &'a str,
        record_key: &'a str,
        node: &'a NodeRow,
        shape: &ShapeNode,
    ) -> Result<Self, String> {
        let node_key = node_key(pod, tool, &node.node_id);
        let root = root_key(&node_key);
        let mut plan = Plan {
            pod,
            tool,
            zone: zone(pod, tool),
            record_key,
            node,
            node_key,
            root: root.clone(),
            values: Vec::new(),
            members: Vec::new(),
            items: Vec::new(),
        };
        plan.object(shape, &node.value, &root, "value")?;
        Ok(plan)
    }

    fn subject_key(&self) -> Option<String> {
        self.node.subject.as_ref().map(|s| key(self.pod, &s.id))
    }

    fn readers(&self) -> String {
        self.node.readers.join(&SEP.to_string())
    }

    fn object(
        &mut self,
        shape: &ShapeNode,
        data: &Value,
        id: &str,
        path: &str,
    ) -> Result<(), String> {
        let Some(data) = data.as_object() else {
            return Err(format!("{path} must be an object"));
        };
        if let Some(extra) = data.keys().find(|k| shape.field(k).is_none()) {
            return Err(format!(
                "{path}.{extra} is not a field of {}",
                shape.type_info.name
            ));
        }
        self.values.push(ValueRow {
            value_id: id.to_owned(),
            kind: "object",
            payload: Payload::None,
        });
        for field in &shape.fields {
            let Some(value) = data.get(&field.member_key).filter(|v| !v.is_null()) else {
                continue;
            };
            let child_path = format!("{path}.{}", field.member_key);
            let child_id = value_id(id, &field.member_key, field.ordinal);
            self.members.push(MemberRow {
                from: id.to_owned(),
                to: child_id.clone(),
                field_id: field.field_id.0.clone(),
                member_key: field.member_key.clone(),
                ordinal: field.ordinal,
            });
            if field.cardinality == Cardinality::Many {
                let Some(items) = value.as_array() else {
                    return Err(format!("{child_path} must be a list"));
                };
                self.values.push(ValueRow {
                    value_id: child_id.clone(),
                    kind: "list",
                    payload: Payload::None,
                });
                for (index, item) in (0_i64..).zip(items) {
                    let item_id = value_id(&child_id, "item", index);
                    self.field_value(field, item, &item_id, &format!("{child_path}[{index}]"))?;
                    self.items.push(ItemRow {
                        from: child_id.clone(),
                        to: item_id,
                        index,
                    });
                }
            } else {
                self.field_value(field, value, &child_id, &child_path)?;
            }
        }
        Ok(())
    }

    fn field_value(
        &mut self,
        field: &FieldInfo,
        value: &Value,
        id: &str,
        path: &str,
    ) -> Result<(), String> {
        if let Some(child) = field.child_shape.as_deref() {
            return self.object(child, value, id, path);
        }
        let (kind, payload) = if field.is_reference() {
            let reference = value
                .as_str()
                .or_else(|| value.get("@id").and_then(Value::as_str))
                .ok_or_else(|| format!("{path} must be a reference"))?;
            ("reference", Payload::Reference(reference.to_owned()))
        } else {
            let primitive = field.value_type.primitive;
            (
                "scalar",
                scalar(primitive, value)
                    .ok_or_else(|| format!("{path} is not a {}", primitive.as_str()))?,
            )
        };
        self.values.push(ValueRow {
            value_id: id.to_owned(),
            kind,
            payload,
        });
        Ok(())
    }
}

/// The plans of one key's nodes, and the ids of the nodes whose value does
/// not fit its type's shape, which are left out.
fn plan_key<'a>(
    conn: &Connection<'_>,
    shapes: &mut HashMap<TypeId, ShapeNode>,
    pod: &'a str,
    tool: &'a str,
    record_key: &'a str,
    rows: &'a KeyRows,
) -> (Vec<Plan<'a>>, HashSet<String>) {
    let mut plans = Vec::with_capacity(rows.nodes.len());
    let mut failed = HashSet::new();
    for node in &rows.nodes {
        let planned = shape_for(conn, shapes, &TypeId::new(&node.type_id))
            .map_err(|e| e.to_string())
            .and_then(|shape| Plan::of(pod, tool, record_key, node, &shape));
        match planned {
            Ok(plan) => plans.push(plan),
            Err(_) => {
                failed.insert(node.node_id.clone());
            }
        }
    }
    (plans, failed)
}

/// One relation as the graph holds it.
struct LinkPlan {
    link_key: String,
    pod: String,
    zone: String,
    record_key: String,
    from_key: String,
    name: String,
    to_key: String,
    to_type: String,
}

/// The relations of one key, but those from a node left out.
fn link_rows(
    pod: &str,
    tool: &str,
    record_key: &str,
    rows: &KeyRows,
    failed: &HashSet<String>,
) -> Vec<LinkPlan> {
    rows.links
        .iter()
        .filter(|l| !failed.contains(&l.from))
        .map(|l| {
            let (from_key, to_key) = (
                node_key(pod, tool, &l.from),
                node_key(pod, &l.to_tool, &l.to),
            );
            LinkPlan {
                link_key: format!("{from_key}{SEP}{}{SEP}{to_key}", l.name),
                pod: pod.to_owned(),
                zone: zone(pod, tool),
                record_key: record_key.to_owned(),
                from_key,
                name: l.name.clone(),
                to_key,
                to_type: l.to_type.clone(),
            }
        })
        .collect()
}

fn scalar(primitive: Primitive, value: &Value) -> Option<Payload> {
    Some(match primitive {
        Primitive::String | Primitive::Reference => Payload::String(value.as_str()?.to_owned()),
        Primitive::Bool => Payload::Bool(value.as_bool()?),
        Primitive::Int => Payload::Int(value.as_i64()?),
        Primitive::Float => Payload::Float(value.as_f64()?),
        Primitive::Decimal => Payload::Decimal(match value {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => return None,
        }),
        Primitive::Date => Payload::Date(value.as_str()?.to_owned()),
        Primitive::Timestamp => Payload::Timestamp(value.as_str()?.to_owned()),
    })
}

/// The statements a key is written with.
struct Statements {
    subject_exists: PreparedStatement,
    subject: PreparedStatement,
    subject_type: PreparedStatement,
    node: PreparedStatement,
    node_about: PreparedStatement,
    node_value: PreparedStatement,
    value_type: PreparedStatement,
    member: PreparedStatement,
    item: PreparedStatement,
    link: PreparedStatement,
}

impl Statements {
    fn prepare(conn: &Connection<'_>) -> Result<Self, String> {
        let p = |q: &str| conn.prepare(q).map_err(|e| format!("{e} — in: {q}"));
        Ok(Self {
            subject_exists: p("MATCH (s:Subject) WHERE s.subject_key = $key RETURN count(s);")?,
            subject: p("CREATE (:Subject {subject_key: $key, pod_id: $pod, subject_id: $id, display_name: $name});")?,
            subject_type: p(
                "MATCH (s:Subject), (t:TypeDef) WHERE s.subject_key = $from AND t.type_id = $to CREATE (s)-[:SUBJECT_HAS_TYPE]->(t);",
            )?,
            node: p(
                "CREATE (:Node {node_key: $key, pod_id: $pod, tool: $tool, zone: $zone, record_key: $record, node_id: $id, issued_by: $by, issued_at: $at, issued_on: $device, readers: $readers});",
            )?,
            node_about: p(
                "MATCH (c:Node), (s:Subject) WHERE c.node_key = $from AND s.subject_key = $to CREATE (c)-[:NODE_ABOUT]->(s);",
            )?,
            node_value: p(
                "MATCH (c:Node), (v:ValueNode) WHERE c.node_key = $from AND v.value_id = $to CREATE (c)-[:NODE_HAS_VALUE]->(v);",
            )?,
            value_type: p(
                "MATCH (v:ValueNode), (t:TypeDef) WHERE v.value_id = $from AND t.type_id = $to CREATE (v)-[:VALUE_HAS_TYPE]->(t);",
            )?,
            member: p(
                "MATCH (a:ValueNode), (b:ValueNode) WHERE a.value_id = $from AND b.value_id = $to
                 CREATE (a)-[:VALUE_HAS_OBJECT_MEMBER {containment_id: $cid, field_id: $field, member_key: $key, ordinal: $ordinal}]->(b);",
            )?,
            item: p(
                "MATCH (a:ValueNode), (b:ValueNode) WHERE a.value_id = $from AND b.value_id = $to
                 CREATE (a)-[:VALUE_HAS_LIST_ITEM {containment_id: $cid, `index`: $item}]->(b);",
            )?,
            link: p(
                "CREATE (:Link {link_key: $key, pod_id: $pod, zone: $zone, record_key: $record, from_key: $from, name: $name, to_key: $to, to_type: $to_type});",
            )?,
        })
    }
}

/// Value nodes, one prepared `CREATE` per union arm, since a `UNION`
/// column cannot be `COPY`d.
struct ValueStatements {
    none: PreparedStatement,
    string: PreparedStatement,
    boolean: PreparedStatement,
    int: PreparedStatement,
    float: PreparedStatement,
    decimal: PreparedStatement,
    date: PreparedStatement,
    timestamp: PreparedStatement,
    reference: PreparedStatement,
}

impl ValueStatements {
    fn prepare(conn: &Connection<'_>) -> Result<Self, String> {
        let p = |arm: &str| {
            let value = if arm.is_empty() {
                "NULL".to_owned()
            } else {
                format!("union_value({arm})")
            };
            let q = format!(
                "CREATE (:ValueNode {{value_id: $id, pod_id: $pod, zone: $zone, record_key: $record, representation_kind: $kind, value: {value}}});"
            );
            conn.prepare(&q).map_err(|e| format!("{e} — in: {q}"))
        };
        Ok(Self {
            none: p("")?,
            string: p("string_value := $v")?,
            boolean: p("bool_value := $v")?,
            int: p("int_value := $v")?,
            float: p("float_value := $v")?,
            decimal: p("decimal_value := CAST($v AS DECIMAL(38, 10))")?,
            date: p("date_value := date($v)")?,
            timestamp: p("timestamp_value := timestamp($v)")?,
            reference: p("ref_subject_id := $v")?,
        })
    }

    fn insert(
        &mut self,
        conn: &Connection<'_>,
        plan: &Plan<'_>,
        row: &ValueRow,
    ) -> Result<(), String> {
        let mut params = vec![
            ("id", s(&row.value_id)),
            ("pod", s(plan.pod)),
            ("zone", s(&plan.zone)),
            ("record", s(plan.record_key)),
            ("kind", s(row.kind)),
        ];
        let statement = match &row.payload {
            Payload::None => &mut self.none,
            Payload::String(v) => {
                params.push(("v", s(v)));
                &mut self.string
            }
            Payload::Bool(b) => {
                params.push(("v", Lv::Bool(*b)));
                &mut self.boolean
            }
            Payload::Int(i) => {
                params.push(("v", Lv::Int64(*i)));
                &mut self.int
            }
            Payload::Float(f) => {
                params.push(("v", Lv::Double(*f)));
                &mut self.float
            }
            Payload::Decimal(v) => {
                params.push(("v", s(v)));
                &mut self.decimal
            }
            Payload::Date(v) => {
                params.push(("v", s(v)));
                &mut self.date
            }
            Payload::Timestamp(v) => {
                params.push(("v", s(v)));
                &mut self.timestamp
            }
            Payload::Reference(v) => {
                params.push(("v", s(v)));
                &mut self.reference
            }
        };
        conn.execute(statement, params).map_err(db_err)?;
        Ok(())
    }
}

fn exec(
    conn: &Connection<'_>,
    statement: &mut PreparedStatement,
    params: Vec<(&str, Lv)>,
) -> Result<(), String> {
    conn.execute(statement, params).map(|_| ()).map_err(db_err)
}

/// Run `work` in one transaction: all of it, or none.
fn in_transaction<T>(
    conn: &Connection<'_>,
    work: impl FnOnce(&Connection<'_>) -> Result<T, String>,
) -> Result<T, String> {
    conn.query("BEGIN TRANSACTION;").map_err(db_err)?;
    match work(conn) {
        Ok(out) => {
            conn.query("COMMIT;").map_err(db_err)?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.query("ROLLBACK;");
            Err(e)
        }
    }
}

/// How many of a key's nodes, and of the relations from the nodes that
/// went in, `LadybugDB` refused.
#[derive(Default)]
struct Refused {
    nodes: usize,
    links: usize,
}

/// Write one key's nodes and relations one transaction each, after the key
/// as a whole was refused: what `LadybugDB` refuses (a date it cannot
/// parse, a decimal out of range) costs that node and its relations alone.
fn append_each(
    conn: &Connection<'_>,
    plans: &[Plan<'_>],
    links: &[LinkPlan],
    known_types: &HashSet<String>,
) -> Refused {
    let mut refused = Refused::default();
    let mut out: HashSet<&str> = HashSet::new();
    for plan in plans {
        let one = std::slice::from_ref(plan);
        if in_transaction(conn, |conn| append(conn, one, &[], known_types)).is_err() {
            refused.nodes += 1;
            out.insert(&plan.node_key);
        }
    }
    for link in links.iter().filter(|l| !out.contains(l.from_key.as_str())) {
        let one = std::slice::from_ref(link);
        if in_transaction(conn, |conn| append(conn, &[], one, known_types)).is_err() {
            refused.links += 1;
        }
    }
    refused
}

/// Write one key's nodes and relations, inside the caller's transaction.
fn append(
    conn: &Connection<'_>,
    plans: &[Plan<'_>],
    links: &[LinkPlan],
    known_types: &HashSet<String>,
) -> Result<(), String> {
    if plans.is_empty() && links.is_empty() {
        return Ok(());
    }
    let mut st = Statements::prepare(conn)?;
    let mut values = ValueStatements::prepare(conn)?;
    for plan in plans {
        append_one(conn, &mut st, &mut values, plan, known_types)
            .map_err(|e| format!("node {}: {e}", plan.node.node_id))?;
    }
    for link in links {
        exec(
            conn,
            &mut st.link,
            vec![
                ("key", s(&link.link_key)),
                ("pod", s(&link.pod)),
                ("zone", s(&link.zone)),
                ("record", s(&link.record_key)),
                ("from", s(&link.from_key)),
                ("name", s(&link.name)),
                ("to", s(&link.to_key)),
                ("to_type", s(&link.to_type)),
            ],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one node: its subject, its row, its value tree and their edges, in order
fn append_one(
    conn: &Connection<'_>,
    st: &mut Statements,
    values: &mut ValueStatements,
    plan: &Plan<'_>,
    known_types: &HashSet<String>,
) -> Result<(), String> {
    let (node, pod) = (plan.node, plan.pod);
    if let (Some(subject), Some(subject_key)) = (&node.subject, plan.subject_key()) {
        let exists = conn
            .execute(&mut st.subject_exists, vec![("key", s(&subject_key))])
            .map_err(db_err)?;
        let exists = exists
            .into_iter()
            .next()
            .and_then(|r| r.first().map(int))
            .unwrap_or(0)
            > 0;
        if !exists {
            exec(
                conn,
                &mut st.subject,
                vec![
                    ("key", s(&subject_key)),
                    ("pod", s(pod)),
                    ("id", s(&subject.id)),
                    ("name", s(&subject.name)),
                ],
            )?;
            if known_types.contains(&subject.type_id) {
                exec(
                    conn,
                    &mut st.subject_type,
                    vec![("from", s(&subject_key)), ("to", s(&subject.type_id))],
                )?;
            }
        }
    }
    exec(
        conn,
        &mut st.node,
        vec![
            ("key", s(&plan.node_key)),
            ("pod", s(pod)),
            ("tool", s(plan.tool)),
            ("zone", s(&plan.zone)),
            ("record", s(plan.record_key)),
            ("id", s(&node.node_id)),
            ("by", s(&node.member)),
            ("at", s(&node.at)),
            ("device", s(node.device.as_deref().unwrap_or_default())),
            ("readers", s(&plan.readers())),
        ],
    )?;
    for row in &plan.values {
        values.insert(conn, plan, row)?;
    }
    if let Some(subject_key) = plan.subject_key() {
        exec(
            conn,
            &mut st.node_about,
            vec![("from", s(&plan.node_key)), ("to", s(&subject_key))],
        )?;
    }
    exec(
        conn,
        &mut st.node_value,
        vec![("from", s(&plan.node_key)), ("to", s(&plan.root))],
    )?;
    exec(
        conn,
        &mut st.value_type,
        vec![("from", s(&plan.root)), ("to", s(&node.type_id))],
    )?;
    for m in &plan.members {
        exec(
            conn,
            &mut st.member,
            vec![
                ("from", s(&m.from)),
                ("to", s(&m.to)),
                ("cid", s(&format!("{}->{}", m.from, m.to))),
                ("field", s(&m.field_id)),
                ("key", s(&m.member_key)),
                ("ordinal", Lv::Int64(m.ordinal)),
            ],
        )?;
    }
    for i in &plan.items {
        exec(
            conn,
            &mut st.item,
            vec![
                ("from", s(&i.from)),
                ("to", s(&i.to)),
                ("cid", s(&format!("{}[{}]->{}", i.from, i.index, i.to))),
                ("item", Lv::Int64(i.index)),
            ],
        )?;
    }
    Ok(())
}

/// Load fresh zones' backlog: node tables by `COPY`, value nodes by
/// prepared `CREATE`, edge tables by `COPY`.
#[allow(clippy::too_many_lines)] // one bulk load, table by table, each before the edges that need it
///
/// Each subject the load makes is pushed to `made` before any table is
/// copied, so a load that fails can take them out again. `fails_at`
/// names a table whose `COPY` fails, for the crate's tests.
fn bulk_load(
    conn: &Connection<'_>,
    plans: &[Plan<'_>],
    links: &[LinkPlan],
    known_types: &HashSet<String>,
    made: &mut Vec<String>,
    fails_at: Option<&str>,
) -> Result<(), String> {
    let dir = Staging::new()?;
    let copy = |table: &str, header: &[&str], rows: &[Vec<String>]| {
        if fails_at == Some(table) {
            return Err(format!("the bulk load is made to fail at {table}"));
        }
        copy(conn, &dir, table, header, rows)
    };
    let mut subjects: Vec<Vec<String>> = Vec::new();
    let mut subject_types: Vec<Vec<String>> = Vec::new();
    let mut seen_subjects = HashSet::new();
    let mut nodes = Vec::new();
    let (mut about, mut has_value, mut value_type, mut members, mut items) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for plan in plans {
        let (n, pod) = (plan.node, plan.pod);
        if let (Some(subject), Some(subject_key)) = (&n.subject, plan.subject_key()) {
            if seen_subjects.insert(subject_key.clone()) && !subject_exists(conn, &subject_key)? {
                made.push(subject_key.clone());
                subjects.push(vec![
                    subject_key.clone(),
                    pod.to_owned(),
                    subject.id.clone(),
                    subject.name.clone(),
                ]);
                if known_types.contains(&subject.type_id) {
                    subject_types.push(vec![subject_key.clone(), subject.type_id.clone()]);
                }
            }
            about.push(vec![plan.node_key.clone(), subject_key]);
        }
        nodes.push(vec![
            plan.node_key.clone(),
            pod.to_owned(),
            plan.tool.to_owned(),
            plan.zone.clone(),
            plan.record_key.to_owned(),
            n.node_id.clone(),
            n.member.clone(),
            n.at.clone(),
            n.device.clone().unwrap_or_default(),
            plan.readers(),
        ]);
        has_value.push(vec![plan.node_key.clone(), plan.root.clone()]);
        value_type.push(vec![plan.root.clone(), n.type_id.clone()]);
        for m in &plan.members {
            members.push(vec![
                m.from.clone(),
                m.to.clone(),
                format!("{}->{}", m.from, m.to),
                m.field_id.clone(),
                m.member_key.clone(),
                m.ordinal.to_string(),
            ]);
        }
        for i in &plan.items {
            items.push(vec![
                i.from.clone(),
                i.to.clone(),
                format!("{}[{}]->{}", i.from, i.index, i.to),
                i.index.to_string(),
            ]);
        }
    }
    let link_rows: Vec<Vec<String>> = links
        .iter()
        .map(|l| {
            vec![
                l.link_key.clone(),
                l.pod.clone(),
                l.zone.clone(),
                l.record_key.clone(),
                l.from_key.clone(),
                l.name.clone(),
                l.to_key.clone(),
                l.to_type.clone(),
            ]
        })
        .collect();

    copy(
        "Subject",
        &["subject_key", "pod_id", "subject_id", "display_name"],
        &subjects,
    )?;
    copy(
        "Node",
        &[
            "node_key",
            "pod_id",
            "tool",
            "zone",
            "record_key",
            "node_id",
            "issued_by",
            "issued_at",
            "issued_on",
            "readers",
        ],
        &nodes,
    )?;
    copy(
        "Link",
        &[
            "link_key",
            "pod_id",
            "zone",
            "record_key",
            "from_key",
            "name",
            "to_key",
            "to_type",
        ],
        &link_rows,
    )?;
    let mut values = ValueStatements::prepare(conn)?;
    in_transaction(conn, |conn| {
        for plan in plans {
            for row in &plan.values {
                values.insert(conn, plan, row)?;
            }
        }
        Ok(())
    })?;
    copy("SUBJECT_HAS_TYPE", &["from", "to"], &subject_types)?;
    copy("NODE_ABOUT", &["from", "to"], &about)?;
    copy("NODE_HAS_VALUE", &["from", "to"], &has_value)?;
    copy("VALUE_HAS_TYPE", &["from", "to"], &value_type)?;
    copy(
        "VALUE_HAS_OBJECT_MEMBER",
        &[
            "from",
            "to",
            "containment_id",
            "field_id",
            "member_key",
            "ordinal",
        ],
        &members,
    )?;
    copy(
        "VALUE_HAS_LIST_ITEM",
        &["from", "to", "containment_id", "index"],
        &items,
    )?;
    Ok(())
}

fn subject_exists(conn: &Connection<'_>, subject_key: &str) -> Result<bool, String> {
    let rows = run(
        conn,
        "MATCH (s:Subject) WHERE s.subject_key = $key RETURN count(s);",
        vec![("key", s(subject_key))],
    )?;
    Ok(rows.first().and_then(|r| r.first()).map_or(0, int) > 0)
}

/// Remove the subjects `keys` name, with their type edges.
fn remove_subjects(conn: &Connection<'_>, keys: &[String]) -> Result<(), String> {
    if keys.is_empty() {
        return Ok(());
    }
    run(
        conn,
        "MATCH (s:Subject) WHERE s.subject_key IN $keys DETACH DELETE s;",
        vec![("keys", list(keys))],
    )?;
    Ok(())
}

/// Remove one zone's nodes, values and relations; every other zone's, the
/// Pod's subjects and the registry stay.
fn clear_zone(conn: &Connection<'_>, pod: &str, tool: &str) -> Result<(), String> {
    let zone = zone(pod, tool);
    for label in ["Node", "ValueNode", "Link"] {
        run(
            conn,
            &format!("MATCH (n:{label}) WHERE n.zone = $zone DETACH DELETE n;"),
            vec![("zone", s(&zone))],
        )?;
    }
    Ok(())
}

/// Remove what one key holds: its nodes, their values and its relations.
/// Subjects stay: other nodes may be about them, and one no node is about
/// is read by nothing.
fn remove_key(
    conn: &Connection<'_>,
    pod: &str,
    tool: &str,
    record_key: &str,
) -> Result<(), String> {
    let zone = zone(pod, tool);
    for label in ["Node", "ValueNode", "Link"] {
        run(
            conn,
            &format!("MATCH (n:{label}) WHERE n.zone = $zone AND n.record_key = $record DETACH DELETE n;"),
            vec![("zone", s(&zone)), ("record", s(record_key))],
        )?;
    }
    Ok(())
}

/// A directory for CSV files, removed when dropped.
struct Staging(PathBuf);

impl Staging {
    fn new() -> Result<Self, String> {
        let dir =
            std::env::temp_dir().join(format!("gqe-staging-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("staging {}: {e}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `rows` as CSV and `COPY` them into `table`. Nothing to copy is not
/// a copy.
fn copy(
    conn: &Connection<'_>,
    dir: &Staging,
    table: &str,
    header: &[&str],
    rows: &[Vec<String>],
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let path = dir.path().join(format!("{table}.csv"));
    let mut csv = String::new();
    csv_line(&mut csv, header.iter().copied());
    for row in rows {
        csv_line(&mut csv, row.iter().map(String::as_str));
    }
    std::fs::write(&path, csv).map_err(|e| format!("writing {}: {e}", path.display()))?;
    // PARALLEL=false: a quoted field may hold a newline, which the
    // parallel reader cannot split around.
    let cypher = format!(
        "COPY {table} FROM '{}' (HEADER=true, PARALLEL=false);",
        path.display()
    );
    conn.query(&cypher)
        .map_err(|e| format!("{e} — in: {cypher}"))?;
    Ok(())
}

/// One CSV line, every field quoted, quotes doubled.
fn csv_line<'a>(out: &mut String, fields: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for field in fields {
        if !first {
            out.push(',');
        }
        first = false;
        if field.is_empty() {
            // Unquoted and empty reads as NULL, which is what an absent
            // optional column is.
            continue;
        }
        out.push('"');
        out.push_str(&field.replace('"', "\"\""));
        out.push('"');
    }
    out.push('\n');
}
