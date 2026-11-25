//! Search by meaning: an owner's texts with the vectors it made, under an HNSW index by cosine
//! distance. A search reaches only what its owner put into that index, and an index leaves nothing
//! behind once dropped. Spec: the vector-index capability of mia-docs' `graph-query-engine`.

use std::collections::HashMap;
use std::sync::RwLock;

use lbug::{Connection, Database, LogicalType, SystemConfig, Value as Lv};

use crate::{GraphError, GraphResult};

/// How many candidates the HNSW search keeps. `LadybugDB`'s default, 200,
/// lost the best match among 50,000 vectors that an exact search ranked
/// first; 500 found it, at 9 ms against 5 on the bench machine.
pub const SEARCH_BREADTH: usize = 1000;

/// One text in an index, as its owner gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// The Pod it is from.
    pub pod: String,
    /// The owner's label for what kind of text it is.
    pub kind: String,
    /// The owner's label for what a citation of it names.
    pub reference: String,
    pub text: String,
}

/// Texts and their vectors, searchable by meaning.
pub struct VectorIndex {
    db: Database,
    dir: TempDir,
    entries: RwLock<HashMap<String, Entry>>,
    dims: usize,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    /// `LadybugDB` keeps a database as one file at its path, with files of its
    /// own beside it named after it (its write-ahead log). All of them go.
    fn drop(&mut self) {
        let path = &self.0;
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return;
        };
        let Ok(siblings) = std::fs::read_dir(parent) else {
            return;
        };
        let prefix = format!("{name}.");
        for sibling in siblings.flatten() {
            if sibling
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
            {
                let _ = std::fs::remove_file(sibling.path());
            }
        }
    }
}

fn unavailable(e: impl std::fmt::Display) -> GraphError {
    GraphError::Unavailable(e.to_string())
}

const INSERT: &str = "CREATE (:Chunk {id: $id, pod: $pod, kind: $kind, text: $text, emb: $emb});";
const CREATE_INDEX: &str =
    "CALL CREATE_VECTOR_INDEX('Chunk', 'chunk_emb', 'emb', metric := 'cosine');";

impl VectorIndex {
    /// An index of vectors of `dims` dimensions, holding `entries`, each
    /// with the vector at the same place in `vectors`.
    pub fn new<V: AsRef<[f32]>>(
        entries: Vec<Entry>,
        vectors: &[V],
        dims: usize,
    ) -> GraphResult<Self> {
        let dir =
            std::env::temp_dir().join(format!("gqe-vectors-{}", uuid::Uuid::new_v4().simple()));
        let db = Database::new(&dir, SystemConfig::default().max_db_size(1 << 32))
            .map_err(unavailable)?;
        let index = Self {
            db,
            dir: TempDir(dir),
            entries: RwLock::new(HashMap::new()),
            dims,
        };
        {
            let conn = Connection::new(&index.db).map_err(unavailable)?;
            conn.query(&format!(
                "CREATE NODE TABLE Chunk(id STRING, pod STRING, kind STRING, text STRING, emb FLOAT[{dims}], PRIMARY KEY(id));"
            ))
            .map_err(unavailable)?;
        }
        index.add(entries, vectors)?;
        Ok(index)
    }

    /// Add entries to the index as it is: the HNSW index takes rows
    /// inserted after it was built.
    pub fn add<V: AsRef<[f32]>>(&self, entries: Vec<Entry>, vectors: &[V]) -> GraphResult<()> {
        if entries.len() != vectors.len() {
            return Err(GraphError::Unavailable(format!(
                "{} entries and {} vectors",
                entries.len(),
                vectors.len()
            )));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let conn = Connection::new(&self.db).map_err(unavailable)?;
        let had_rows = !self.is_empty();
        let mut insert = conn.prepare(INSERT).map_err(unavailable)?;
        for (entry, vector) in entries.iter().zip(vectors) {
            conn.execute(
                &mut insert,
                vec![
                    ("id", Lv::String(entry.id.clone())),
                    ("pod", Lv::String(entry.pod.clone())),
                    ("kind", Lv::String(entry.kind.clone())),
                    ("text", Lv::String(entry.text.clone())),
                    ("emb", float_array(vector.as_ref())),
                ],
            )
            .map_err(unavailable)?;
        }
        if !had_rows {
            conn.query(CREATE_INDEX).map_err(unavailable)?;
        }
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(entries.into_iter().map(|e| (e.id.clone(), e)));
        Ok(())
    }

    /// Where the index keeps its files while it lives.
    pub fn path(&self) -> &std::path::Path {
        &self.dir.0
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// The `k` entries nearest to `vector`, nearest first, with their
    /// cosine distance.
    pub fn nearest(&self, vector: &[f32], k: usize) -> GraphResult<Vec<(Entry, f64)>> {
        self.nearest_with(vector, k, SEARCH_BREADTH)
    }

    /// [`Self::nearest`], the HNSW search keeping `efs` candidates.
    pub fn nearest_with(
        &self,
        vector: &[f32],
        k: usize,
        efs: usize,
    ) -> GraphResult<Vec<(Entry, f64)>> {
        if vector.len() != self.dims {
            return Err(GraphError::Unavailable(format!(
                "a vector of {} dimensions, in an index of {}",
                vector.len(),
                self.dims
            )));
        }
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let conn = Connection::new(&self.db).map_err(unavailable)?;
        let mut query = conn
            .prepare(&format!(
                "CALL QUERY_VECTOR_INDEX('Chunk', 'chunk_emb', $q, $k, efs := {efs}) RETURN node.id, distance ORDER BY distance;"
            ))
            .map_err(unavailable)?;
        let rows = conn
            .execute(
                &mut query,
                vec![
                    ("q", float_array(vector)),
                    ("k", Lv::Int64(i64::try_from(k).unwrap_or(i64::MAX))),
                ],
            )
            .map_err(unavailable)?;
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for row in rows {
            let (Some(Lv::String(id)), Some(distance)) = (row.first(), row.get(1)) else {
                continue;
            };
            let distance = match distance {
                Lv::Double(d) => *d,
                Lv::Float(f) => f64::from(*f),
                _ => f64::NAN,
            };
            if let Some(entry) = entries.get(id) {
                out.push((entry.clone(), distance));
            }
        }
        Ok(out)
    }
}

fn float_array(vector: &[f32]) -> Lv {
    Lv::Array(
        LogicalType::Float,
        vector.iter().map(|x| Lv::Float(*x)).collect(),
    )
}
