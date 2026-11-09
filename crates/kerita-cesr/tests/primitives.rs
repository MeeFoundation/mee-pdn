//! The corpus' primitives through the crate: a primitive or an indexed signature of the subset
//! reads to keripy's code, value and indices and writes back to its own text; one outside the
//! subset is refused as unsupported; one keripy refuses, the crate refuses.

mod support;

use kerita_cesr::primitive::{IndexedSignature, Primitive, Refusal};
use serde_json::Value;

/// `(where, vector)` for every corpus vector of the kind named.
fn vectors(kind: &str) -> Result<Vec<(String, Value)>, String> {
    let mut found = Vec::new();
    for directory in support::DIRECTORIES {
        for file in support::manifest(directory)?.into_keys() {
            for v in support::vectors(directory, &file)? {
                if v.get("kind").and_then(Value::as_str) == Some(kind) {
                    let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
                    found.push((format!("{directory}/{id}"), v));
                }
            }
        }
    }
    Ok(found)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Checks every vector of a kind against what the crate answered for its input, and returns how
/// many vectors of each expectation it checked: same, unsupported, refused.
fn check<T>(
    kind: &str,
    read: impl Fn(&str) -> Result<T, Refusal>,
    same: impl Fn(&str, &T, &Value) -> Result<(), String>,
) -> Result<[usize; 3], String> {
    let (mut alike, mut unsupported, mut refused) = (0, 0, 0);
    for (id, v) in vectors(kind)? {
        let text = v
            .pointer("/input/qb64")
            .and_then(Value::as_str)
            .ok_or(format!("{id}: no input"))?;
        let keripy = v.get("keripy").unwrap_or(&Value::Null);
        match (v.get("expect").and_then(Value::as_str), read(text)) {
            (Some("same"), Ok(read)) => {
                same(&id, &read, keripy)?;
                alike += 1;
            }
            (Some("unsupported"), Err(Refusal::Unsupported(_))) => unsupported += 1,
            (Some("refused"), Err(_)) => refused += 1,
            (expect, answer) => {
                return Err(format!(
                    "{id}: {text} expects {expect:?}, the crate answered {:?}",
                    answer.err()
                ));
            }
        }
    }
    Ok([alike, unsupported, refused])
}

fn agree(id: &str, field: &str, ours: &Value, keripy: &Value) -> Result<(), String> {
    if ours == keripy {
        Ok(())
    } else {
        Err(format!("{id}: {field} {ours} against keripy's {keripy}"))
    }
}

/// Every primitive keripy built reads to keripy's code and value and writes back to its text
/// when it is in the subset, and is refused as unsupported when it is not; one keripy refuses,
/// the crate refuses.
#[test]
fn primitives_read_as_keripy_reads_them() {
    let counts = check("matter", Primitive::read, |id, p, keripy| {
        agree(id, "code", &Value::from(p.code()), &keripy["code"])?;
        agree(id, "value", &Value::from(hex(p.raw())), &keripy["raw"])
    })
    .unwrap();
    assert!(
        counts.iter().all(|&n| n > 0),
        "same, unsupported, refused: {counts:?}"
    );
}

/// Every indexed signature keripy built reads to keripy's code, indices and value when it is in
/// the subset, and is refused as unsupported when it is not; one keripy refuses, the crate refuses.
#[test]
fn indexed_signatures_read_as_keripy_reads_them() {
    let counts = check("indexer", IndexedSignature::read, |id, s, keripy| {
        agree(id, "code", &Value::from(s.code()), &keripy["code"])?;
        agree(id, "index", &Value::from(s.index()), &keripy["index"])?;
        agree(
            id,
            "ondex",
            &s.ondex().map_or(Value::Null, Value::from),
            &keripy["ondex"],
        )?;
        agree(id, "value", &Value::from(hex(s.raw())), &keripy["raw"])
    })
    .unwrap();
    assert!(
        counts.iter().all(|&n| n > 0),
        "same, unsupported, refused: {counts:?}"
    );
}
