//! The registry the graph is typed by: mia-ontologies' tables with the application ontology on top,
//! parsed and checked here and loaded into the graph by [`crate::db`]. The files under `ontology/`
//! are snapshots, and regenerating them is how the crate learns a new type. Spec: the ontology-
//! registry capability of mia-docs' `graph-query-engine`.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    source: String,
    types: Vec<TypeRow>,
    fields: Vec<FieldRow>,
    type_has_field: Vec<(String, String)>,
    field_value_type: Vec<(String, String)>,
    /// (`type_id`, the `type_id` it extends). mia-ontologies' registry has none.
    #[serde(default)]
    extends: Vec<(String, String)>,
}

/// One `TypeDef` row, as the registry has it.
#[derive(Deserialize, Clone)]
pub struct TypeRow {
    pub type_id: String,
    pub name: String,
    pub canonical_name: Option<String>,
    pub description: Option<String>,
    pub kind: String,
    pub primitive_representation: Option<String>,
}

/// One `FieldSpec` row, as the registry has it.
#[derive(Deserialize, Clone)]
pub struct FieldRow {
    pub field_id: String,
    pub member_key: String,
    pub display_name: String,
    pub required: bool,
    pub cardinality: String,
    pub ordinal: i64,
}

/// The registry's four tables, row for row — what a graph database is
/// loaded with.
pub struct RegistryTables {
    pub types: Vec<TypeRow>,
    pub fields: Vec<FieldRow>,
    /// `TYPE_HAS_FIELD`: (`type_id`, `field_id`).
    pub type_has_field: Vec<(String, String)>,
    /// `FIELD_VALUE_TYPE`: (`field_id`, `type_id`).
    pub field_value_type: Vec<(String, String)>,
    /// `TYPE_EXTENDS`: (`type_id`, the more general `type_id`).
    pub extends: Vec<(String, String)>,
}

/// A loaded ontology registry: the schema a graph database is loaded
/// with. It answers no questions itself — types, shapes and forms are read
/// from the graph it is loaded into, so there is one place they come from.
pub struct Ontology {
    source: String,
    tables: RegistryTables,
}

impl Ontology {
    /// Load a registry from its JSON form. Every edge must name a type and
    /// a field that exist, and every field must have a value type: a
    /// registry that breaks either is refused here rather than loaded into
    /// a graph with a hole in it.
    pub fn from_registry_json(json: &str) -> Result<Self, String> {
        Self::layered(&[json])
    }

    /// Several registries loaded as one, each on top of those before it: a
    /// later one may use, and extend, an earlier one's types, and may not
    /// define a type or field again.
    #[allow(clippy::too_many_lines)] // every check of a registry, in the order a refusal names the first fault
    pub fn layered(jsons: &[&str]) -> Result<Self, String> {
        let mut registry = Registry {
            source: String::new(),
            types: Vec::new(),
            fields: Vec::new(),
            type_has_field: Vec::new(),
            field_value_type: Vec::new(),
            extends: Vec::new(),
        };
        let mut sources = Vec::new();
        for json in jsons {
            let layer: Registry =
                serde_json::from_str(json).map_err(|e| format!("ontology registry: {e}"))?;
            sources.push(layer.source);
            registry.types.extend(layer.types);
            registry.fields.extend(layer.fields);
            registry.type_has_field.extend(layer.type_has_field);
            registry.field_value_type.extend(layer.field_value_type);
            registry.extends.extend(layer.extends);
        }
        registry.source = sources.join(" + ");
        {
            let mut seen = HashSet::new();
            if let Some(t) = registry
                .types
                .iter()
                .find(|t| !seen.insert(t.type_id.as_str()))
            {
                return Err(format!("type '{}' is defined twice", t.type_id));
            }
            let mut seen = HashSet::new();
            if let Some(f) = registry
                .fields
                .iter()
                .find(|f| !seen.insert(f.field_id.as_str()))
            {
                return Err(format!("field '{}' is defined twice", f.field_id));
            }
        }
        let types: HashSet<&str> = registry.types.iter().map(|t| t.type_id.as_str()).collect();
        let fields: HashSet<&str> = registry
            .fields
            .iter()
            .map(|f| f.field_id.as_str())
            .collect();

        for (type_id, field_id) in &registry.type_has_field {
            if !types.contains(type_id.as_str()) {
                return Err(format!("TYPE_HAS_FIELD names unknown type '{type_id}'"));
            }
            if !fields.contains(field_id.as_str()) {
                return Err(format!("TYPE_HAS_FIELD names unknown field '{field_id}'"));
            }
        }
        let mut with_value_type: HashSet<&str> = HashSet::new();
        for (field_id, type_id) in &registry.field_value_type {
            if !fields.contains(field_id.as_str()) {
                return Err(format!("FIELD_VALUE_TYPE names unknown field '{field_id}'"));
            }
            if !types.contains(type_id.as_str()) {
                return Err(format!("FIELD_VALUE_TYPE names unknown type '{type_id}'"));
            }
            with_value_type.insert(field_id.as_str());
        }
        for (specific, general) in &registry.extends {
            for type_id in [specific, general] {
                if !types.contains(type_id.as_str()) {
                    return Err(format!("EXTENDS names unknown type '{type_id}'"));
                }
            }
        }
        // A leaf must resolve to a type: invariant 12 of the flexible
        // ontology. A field with no value type cannot be validated or
        // queried, so the registry is refused rather than half-loaded.
        for (_, field_id) in &registry.type_has_field {
            if !with_value_type.contains(field_id.as_str()) {
                return Err(format!("field '{field_id}' has no FIELD_VALUE_TYPE"));
            }
        }

        Ok(Self {
            source: registry.source,
            tables: RegistryTables {
                types: registry.types,
                fields: registry.fields,
                type_has_field: registry.type_has_field,
                field_value_type: registry.field_value_type,
                extends: registry.extends,
            },
        })
    }

    /// mia-ontologies with the application ontology on top: what every
    /// graph database here is loaded with.
    pub fn platform() -> Arc<Self> {
        static PLATFORM: OnceLock<Arc<Ontology>> = OnceLock::new();
        #[allow(clippy::expect_used)] // the crate's own files, which its tests load
        Arc::clone(PLATFORM.get_or_init(|| {
            Arc::new(
                Ontology::layered(&[
                    include_str!("../ontology/mee-registry.json"),
                    include_str!("../ontology/app-ontology.json"),
                ])
                .expect("the bundled ontologies load"),
            )
        }))
    }

    /// The mee ontology alone, from the snapshot this crate carries.
    pub fn mee() -> Arc<Self> {
        static MEE: OnceLock<Arc<Ontology>> = OnceLock::new();
        #[allow(clippy::expect_used)] // the crate's own file, which its tests load
        Arc::clone(MEE.get_or_init(|| {
            Arc::new(
                Ontology::from_registry_json(include_str!("../ontology/mee-registry.json"))
                    .expect("the bundled ontology snapshot loads"),
            )
        }))
    }

    /// The registry's tables, for loading into a graph database.
    pub fn tables(&self) -> &RegistryTables {
        &self.tables
    }

    /// Where this registry came from, as recorded in it.
    pub fn source(&self) -> &str {
        &self.source
    }
}
