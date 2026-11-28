//! A query built from a shape: a type, filters and a limit, refused when it is built if a path or a
//! filter does not fit the shape, so no query quietly matches nothing.

use serde_json::Value;

use super::shape::{
    Cardinality, FilterCondition, FilterOp, Primitive, ShapeError, ShapeNode, TypeId,
};

/// The default row limit, matching the Python builder's own default.
pub const DEFAULT_LIMIT: usize = 50;

/// A query over one entity type. Immutable: every method returns a new
/// query, so a partially built query can be shared and specialized.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityQuery {
    shape: ShapeNode,
    filters: Vec<FilterCondition>,
    limit: usize,
}

impl EntityQuery {
    /// Start a query from a loaded shape.
    pub fn new(shape: ShapeNode) -> Self {
        Self {
            shape,
            filters: Vec::new(),
            limit: DEFAULT_LIMIT,
        }
    }

    pub fn shape(&self) -> &ShapeNode {
        &self.shape
    }

    pub fn type_id(&self) -> &TypeId {
        &self.shape.type_info.type_id
    }

    pub fn filters(&self) -> &[FilterCondition] {
        &self.filters
    }

    pub fn limit_value(&self) -> usize {
        self.limit
    }

    /// The field keys available at the top level, for a caller deciding
    /// what to filter on.
    pub fn keys(&self) -> Vec<&str> {
        self.shape.member_keys()
    }

    /// Add a filter. The path is resolved against the shape and the
    /// operation is checked against what that field accepts, so both
    /// failures are refusals with the alternatives named.
    pub fn filter(
        mut self,
        path: &[impl AsRef<str>],
        op: FilterOp,
        value: Value,
    ) -> Result<Self, ShapeError> {
        let resolved = self.shape.resolve_path(path)?;
        let Some(leaf) = resolved.last() else {
            return Err(ShapeError::EmptyPath);
        };

        let supported = leaf.supported_filters();
        if !supported.contains(&op) {
            return Err(ShapeError::UnsupportedFilter {
                member_key: leaf.member_key.clone(),
                op,
                available: supported,
            });
        }

        let primitive = if leaf.is_reference() {
            Primitive::Reference
        } else {
            leaf.value_type.primitive
        };
        check_value_type(&leaf.member_key, primitive, &value)?;

        self.filters.push(FilterCondition {
            path: resolved,
            op,
            value,
        });
        Ok(self)
    }

    /// Convenience for the commonest case: equality on a top-level field.
    pub fn filter_eq(self, member_key: &str, value: Value) -> Result<Self, ShapeError> {
        self.filter(&[member_key], FilterOp::Eq, value)
    }

    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }

    /// Whether an entity document satisfies every filter. This is the
    /// definition of what a filter *means*, kept with the query rather
    /// than inside any one store, so two implementations cannot disagree
    /// about it.
    pub fn matches(&self, document: &Value) -> bool {
        self.filters
            .iter()
            .all(|condition| filter_matches(condition, document))
    }
}

fn check_value_type(
    member_key: &str,
    primitive: Primitive,
    value: &Value,
) -> Result<(), ShapeError> {
    let ok = match primitive {
        Primitive::String | Primitive::Date | Primitive::Timestamp | Primitive::Reference => {
            value.is_string()
        }
        Primitive::Bool => value.is_boolean(),
        Primitive::Int => value.is_i64() || value.is_u64(),
        Primitive::Float | Primitive::Decimal => value.is_number(),
    };
    if ok {
        Ok(())
    } else {
        Err(ShapeError::WrongValueType {
            member_key: member_key.to_owned(),
            expected: primitive,
            got: json_type_name(value).to_owned(),
        })
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// Walk a condition's path through a document, collecting every value the
/// path reaches. A `many` step fans out, which is what makes `contains`
/// mean "some item equals this".
fn collect_at_path(condition: &FilterCondition, document: &Value) -> Vec<Value> {
    let mut current = vec![document.clone()];
    for field in &condition.path {
        let mut next = Vec::new();
        for node in &current {
            let Some(value) = node.get(&field.member_key) else {
                continue;
            };
            match field.cardinality {
                Cardinality::Many => {
                    if let Some(items) = value.as_array() {
                        next.extend(items.iter().cloned());
                    }
                }
                Cardinality::One => next.push(value.clone()),
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

fn filter_matches(condition: &FilterCondition, document: &Value) -> bool {
    let found = collect_at_path(condition, document);
    found
        .iter()
        .any(|candidate| compare(candidate, condition.op, &condition.value))
}

fn compare(candidate: &Value, op: FilterOp, target: &Value) -> bool {
    match op {
        FilterOp::Eq | FilterOp::Contains => candidate == target,
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
            match ordering(candidate, target) {
                Some(std::cmp::Ordering::Less) => matches!(op, FilterOp::Lt | FilterOp::Lte),
                Some(std::cmp::Ordering::Equal) => matches!(op, FilterOp::Gte | FilterOp::Lte),
                Some(std::cmp::Ordering::Greater) => matches!(op, FilterOp::Gt | FilterOp::Gte),
                None => false,
            }
        }
    }
}

/// Ordered comparison for the primitives that have one. Dates and
/// timestamps are compared as strings, which is correct for the ISO-8601
/// forms the ontology uses and wrong for anything else — so anything else
/// is not an ordered primitive.
fn ordering(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::String(a), Value::String(b)) => Some(a.as_str().cmp(b.as_str())),
        _ => None,
    }
}
