//! The value check: whether a value fits a type's shape, refusing by name an unknown field, a
//! missing required one, a wrong cardinality or a wrong primitive, and coercing nothing. Spec: the
//! ontology-registry capability of mia-docs' `graph-query-engine`.

use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, PrimitiveDateTime};

use super::shape::{Cardinality, FieldInfo, Primitive, ShapeNode, TypeKind};
use super::{GraphError, GraphResult};

/// The engine's value check: whether `value` may be written as a node of
/// the type `shape` is the shape of. A pod-tool that writes typed values
/// asks this before it writes its record. Refused, naming the field at
/// fault, as [`conform`] refuses; and a type with no fields takes no value.
/// Returns the value as it will be stored.
pub fn check(shape: &ShapeNode, value: &Value) -> GraphResult<Value> {
    if shape.fields.is_empty() {
        return Err(GraphError::NonConformant {
            path: "value".into(),
            reason: format!(
                "'{}' has no fields, so no value can be written with it",
                shape.type_info.name
            ),
        });
    }
    conform(shape, value, "value")
}

/// Check `value` against `shape`, returning the value that will be stored.
/// `path` names where the value sits, for the refusal.
pub fn conform(shape: &ShapeNode, value: &Value, path: &str) -> GraphResult<Value> {
    let Some(object) = value.as_object() else {
        return Err(GraphError::NonConformant {
            path: path.to_owned(),
            reason: "must be an object".into(),
        });
    };

    let allowed: Vec<&str> = shape.member_keys();
    let mut unexpected: Vec<&String> = object
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .collect();
    unexpected.sort();
    if let Some(first) = unexpected.first() {
        return Err(GraphError::NonConformant {
            path: format!("{path}.{first}"),
            reason: format!(
                "no such field on '{}' — available: {}",
                shape.type_info.name,
                allowed.join(", ")
            ),
        });
    }

    let mut out = Map::new();
    for field in &shape.fields {
        let field_path = format!("{path}.{}", field.member_key);
        let Some(raw) = object.get(&field.member_key) else {
            if field.required {
                return Err(GraphError::NonConformant {
                    path: field_path,
                    reason: "is required".into(),
                });
            }
            continue;
        };

        let stored = match field.cardinality {
            Cardinality::Many => {
                let Some(items) = raw.as_array() else {
                    return Err(GraphError::NonConformant {
                        path: field_path,
                        reason: "must be a list".into(),
                    });
                };
                let mut list = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    list.push(conform_field(
                        field,
                        item,
                        &format!("{field_path}[{index}]"),
                    )?);
                }
                Value::Array(list)
            }
            Cardinality::One => conform_field(field, raw, &field_path)?,
        };
        out.insert(field.member_key.clone(), stored);
    }
    Ok(Value::Object(out))
}

fn conform_field(field: &FieldInfo, value: &Value, path: &str) -> GraphResult<Value> {
    if let Some(child) = field.child_shape.as_deref() {
        return conform(child, value, path);
    }
    if field.is_reference() {
        let refusal = || GraphError::NonConformant {
            path: path.to_owned(),
            reason: "must be a reference id string or an object with an '@id' string".into(),
        };
        return match value {
            Value::String(s) => Ok(Value::String(s.clone())),
            Value::Object(map) => map
                .get("@id")
                .and_then(Value::as_str)
                .map(|id| Value::String(id.into()))
                .ok_or_else(refusal),
            _ => Err(refusal()),
        };
    }
    if field.value_type.kind != TypeKind::Scalar {
        return Err(GraphError::NonConformant {
            path: path.to_owned(),
            reason: format!("has unsupported type kind {:?}", field.value_type.kind),
        });
    }
    conform_primitive(field.value_type.primitive, value, path)
}

fn conform_primitive(primitive: Primitive, value: &Value, path: &str) -> GraphResult<Value> {
    let ok = match primitive {
        Primitive::String | Primitive::Reference => value.is_string(),
        Primitive::Date => value.as_str().is_some_and(is_date),
        Primitive::Timestamp => value.as_str().is_some_and(is_timestamp),
        Primitive::Bool => value.is_boolean(),
        Primitive::Int => value.is_i64() || value.is_u64(),
        Primitive::Float => value.is_number(),
        Primitive::Decimal => value.as_f64().is_some_and(fits_decimal),
    };
    if ok {
        Ok(value.clone())
    } else {
        Err(GraphError::NonConformant {
            path: path.to_owned(),
            reason: format!("must be {}", primitive.as_str()),
        })
    }
}

/// `2026-11-21`.
fn is_date(text: &str) -> bool {
    Date::parse(text, format_description!("[year]-[month]-[day]")).is_ok()
}

/// `2026-11-21T10:00:00Z` (RFC 3339), or without a zone, as UTC.
fn is_timestamp(text: &str) -> bool {
    let zoneless = [
        format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second][optional [.[subsecond]]]"
        ),
        format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second][optional [.[subsecond]]]"
        ),
    ];
    OffsetDateTime::parse(text, &Rfc3339).is_ok()
        || zoneless
            .iter()
            .any(|format| PrimitiveDateTime::parse(text, format).is_ok())
}

/// Within the graph's `DECIMAL(38, 10)`: at most 28 digits before the point.
fn fits_decimal(number: f64) -> bool {
    number.is_finite() && number.abs() < 1e28
}
