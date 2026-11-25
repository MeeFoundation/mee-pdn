//! Shapes: a type's fields, and theirs, read from the registry, which query code works from instead
//! of naming an ontology class.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

string_id!(
    TypeId,
    "Stable identifier of an entity type. Addresses a type; `TypeInfo::name` is for display only."
);
string_id!(FieldId, "Stable identifier of one field of a type.");
string_id!(
    ValueId,
    "Identifier of one value node — the root of an entity, or a node nested inside it."
);
string_id!(SubjectId, "Identifier of the party an entity is about.");
string_id!(
    GraphClaimId,
    "Identifier of one claim carrying an entity value."
);

/// What sort of thing a type is. Mirrors `TypeDef.kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeKind {
    /// A structured type with fields of its own.
    Entity,
    /// A leaf carrying a primitive value.
    Scalar,
    /// A kind this vocabulary does not recognize. Reported, never dropped —
    /// the same rule the pod-tool interface applies to an unknown tool kind.
    Unknown(String),
}

impl TypeKind {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "entity" => Self::Entity,
            "scalar" => Self::Scalar,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// How a primitive leaf is represented. Mirrors
/// `TypeDef.primitive_representation`, normalized the way
/// `normalized_primitive` does in the Python: absent means string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Primitive {
    String,
    Bool,
    Int,
    Float,
    Decimal,
    Date,
    Timestamp,
    /// A leaf that points at a subject rather than carrying a value.
    Reference,
}

impl Primitive {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("string").to_ascii_lowercase().as_str() {
            "bool" | "boolean" => Self::Bool,
            "int" | "integer" => Self::Int,
            "float" => Self::Float,
            "decimal" => Self::Decimal,
            "date" => Self::Date,
            "timestamp" => Self::Timestamp,
            "ref_subject_id" | "reference" => Self::Reference,
            _ => Self::String,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Reference => "reference",
        }
    }

    /// Ordered comparison is meaningful for these, so they accept the
    /// range filters as well as equality.
    pub fn is_ordered(&self) -> bool {
        matches!(
            self,
            Self::Int | Self::Float | Self::Decimal | Self::Date | Self::Timestamp
        )
    }
}

/// Whether a field holds one value or a list of them. Mirrors
/// `FieldSpec.cardinality`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cardinality {
    One,
    Many,
}

impl Cardinality {
    pub fn parse(raw: &str) -> Self {
        if raw == "many" {
            Self::Many
        } else {
            Self::One
        }
    }
}

/// One entity type. Mirrors the Python `TypeInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeInfo {
    pub type_id: TypeId,
    /// For display and for human-readable matching. Never an address.
    pub name: String,
    pub canonical_name: Option<String>,
    pub description: Option<String>,
    pub kind: TypeKind,
    pub primitive: Primitive,
}

impl TypeInfo {
    pub fn entity(type_id: impl Into<TypeId>, name: impl Into<String>) -> Self {
        Self {
            type_id: type_id.into(),
            name: name.into(),
            canonical_name: None,
            description: None,
            kind: TypeKind::Entity,
            primitive: Primitive::String,
        }
    }

    pub fn scalar(
        type_id: impl Into<TypeId>,
        name: impl Into<String>,
        primitive: Primitive,
    ) -> Self {
        Self {
            type_id: type_id.into(),
            name: name.into(),
            canonical_name: None,
            description: None,
            kind: TypeKind::Scalar,
            primitive,
        }
    }

    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// One field of a type. Mirrors the Python `FieldInfo`, including its
/// `child_shape`: present when the field's value type is an entity that
/// has fields of its own, absent when the field is a leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldInfo {
    pub field_id: FieldId,
    /// The human-readable key this field is written under in a document.
    pub member_key: String,
    pub display_name: String,
    pub required: bool,
    pub cardinality: Cardinality,
    pub ordinal: i64,
    pub value_type: TypeInfo,
    pub child_shape: Option<Box<ShapeNode>>,
}

impl FieldInfo {
    pub fn leaf(
        member_key: impl Into<String>,
        value_type: TypeInfo,
        cardinality: Cardinality,
        required: bool,
        ordinal: i64,
    ) -> Self {
        let key = member_key.into();
        Self {
            field_id: FieldId::new(format!("field:{key}")),
            display_name: key.clone(),
            member_key: key,
            required,
            cardinality,
            ordinal,
            value_type,
            child_shape: None,
        }
    }

    pub fn nested(
        member_key: impl Into<String>,
        child: ShapeNode,
        cardinality: Cardinality,
        required: bool,
        ordinal: i64,
    ) -> Self {
        let key = member_key.into();
        Self {
            field_id: FieldId::new(format!("field:{key}")),
            display_name: key.clone(),
            member_key: key,
            required,
            cardinality,
            ordinal,
            value_type: child.type_info.clone(),
            child_shape: Some(Box::new(child)),
        }
    }

    /// An entity-typed leaf with no shape beneath it points at a subject
    /// rather than containing one. Mirrors `is_reference_leaf`.
    pub fn is_reference(&self) -> bool {
        self.value_type.kind == TypeKind::Entity && self.child_shape.is_none()
    }

    /// A field carrying a value rather than a nested structure.
    pub fn is_literal(&self) -> bool {
        self.value_type.kind == TypeKind::Scalar || self.is_reference()
    }

    /// Which filters this field accepts. Mirrors
    /// `supported_filter_operations`, and is what makes an unsupported
    /// filter a named refusal rather than a query that returns nothing.
    pub fn supported_filters(&self) -> Vec<FilterOp> {
        if self.cardinality == Cardinality::Many {
            return if self.is_literal() {
                vec![FilterOp::Contains]
            } else {
                Vec::new()
            };
        }
        if self.is_reference() {
            return vec![FilterOp::Eq];
        }
        if self.value_type.kind != TypeKind::Scalar {
            return Vec::new();
        }
        if self.value_type.primitive.is_ordered() {
            vec![
                FilterOp::Eq,
                FilterOp::Gt,
                FilterOp::Gte,
                FilterOp::Lt,
                FilterOp::Lte,
            ]
        } else {
            vec![FilterOp::Eq]
        }
    }
}

/// A type together with its fields, and their fields, recursively. Mirrors
/// the Python `ShapeNode`, which is the single thing every query is
/// generated from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapeNode {
    pub type_info: TypeInfo,
    pub fields: Vec<FieldInfo>,
}

impl ShapeNode {
    pub fn new(type_info: TypeInfo, fields: Vec<FieldInfo>) -> Self {
        Self { type_info, fields }
    }

    pub fn field(&self, member_key: &str) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.member_key == member_key)
    }

    pub fn member_keys(&self) -> Vec<&str> {
        self.fields.iter().map(|f| f.member_key.as_str()).collect()
    }

    /// Resolve a dotted path through nested shapes, e.g. `["address",
    /// "city"]`. Returns the field at each step, so a caller can see what
    /// it traversed. Mirrors the Python's chained `builder["a"]["b"]`.
    pub fn resolve_path(&self, path: &[impl AsRef<str>]) -> Result<Vec<FieldInfo>, ShapeError> {
        if path.is_empty() {
            return Err(ShapeError::EmptyPath);
        }
        let mut resolved = Vec::with_capacity(path.len());
        let mut current = self;
        for (depth, segment) in path.iter().enumerate() {
            let key = segment.as_ref();
            let Some(field) = current.field(key) else {
                return Err(ShapeError::NoSuchField {
                    type_name: current.type_info.name.clone(),
                    member_key: key.to_owned(),
                    available: current
                        .member_keys()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                });
            };
            resolved.push(field.clone());
            let is_last = depth == path.len() - 1;
            if !is_last {
                let Some(child) = field.child_shape.as_deref() else {
                    return Err(ShapeError::NotTraversable {
                        member_key: key.to_owned(),
                        remaining: path
                            .get(depth + 1..)
                            .unwrap_or_default()
                            .iter()
                            .map(|s| s.as_ref().to_owned())
                            .collect(),
                    });
                };
                current = child;
            }
        }
        Ok(resolved)
    }
}

/// The comparisons a filter can make. Mirrors the Python's filter
/// operation names exactly, so the two APIs describe the same queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterOp {
    Eq,
    Contains,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl FilterOp {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "eq" => Some(Self::Eq),
            "contains" => Some(Self::Contains),
            "gt" => Some(Self::Gt),
            "gte" => Some(Self::Gte),
            "lt" => Some(Self::Lt),
            "lte" => Some(Self::Lte),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Contains => "contains",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
        }
    }
}

impl fmt::Display for FilterOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// One resolved filter: the path it traverses, the comparison, the value.
/// The path is resolved against a shape before the condition exists, so a
/// condition naming a field that is not there is unrepresentable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterCondition {
    pub path: Vec<FieldInfo>,
    pub op: FilterOp,
    pub value: serde_json::Value,
}

impl FilterCondition {
    /// The member keys this condition traverses, for display.
    pub fn path_keys(&self) -> Vec<&str> {
        self.path.iter().map(|f| f.member_key.as_str()).collect()
    }

    #[allow(clippy::expect_used)] // a condition is made from a resolved path, which is never empty
    pub fn leaf(&self) -> &FieldInfo {
        self.path
            .last()
            .expect("a filter condition always has at least one field")
    }
}

/// What can go wrong reading a shape or building a query against it. Each
/// variant carries what the caller needs to fix it — the available fields,
/// the supported filters — rather than only saying no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    EmptyPath,
    NoSuchField {
        type_name: String,
        member_key: String,
        available: Vec<String>,
    },
    NotTraversable {
        member_key: String,
        remaining: Vec<String>,
    },
    UnsupportedFilter {
        member_key: String,
        op: FilterOp,
        available: Vec<FilterOp>,
    },
    WrongValueType {
        member_key: String,
        expected: Primitive,
        got: String,
    },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "a filter path must name at least one field"),
            Self::NoSuchField { type_name, member_key, available } => write!(
                f,
                "'{member_key}' is not a field of '{type_name}' — available fields: {}",
                available.join(", ")
            ),
            Self::NotTraversable { member_key, remaining } => write!(
                f,
                "'{member_key}' is a leaf field with no nested shape, so it cannot be traversed into ({})",
                remaining.join(".")
            ),
            Self::UnsupportedFilter { member_key, op, available } => {
                let names: Vec<&str> = available.iter().map(FilterOp::as_str).collect();
                if names.is_empty() {
                    write!(f, "'{member_key}' accepts no filters, so '{op}' cannot be applied to it")
                } else {
                    write!(f, "'{op}' is not a supported filter for '{member_key}' — available: {}", names.join(", "))
                }
            }
            Self::WrongValueType { member_key, expected, got } => {
                write!(f, "filter value for '{member_key}' must be {}, got {got}", expected.as_str())
            }
        }
    }
}

impl std::error::Error for ShapeError {}
