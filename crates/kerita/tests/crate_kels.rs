//! The KELs the crate writes, as committed under `tests/corpus/crate/` for keripy to fold: a fresh
//! build writes the same bytes. With `KERITA_WRITE_CORPUS=1` the test writes them instead, for
//! `just keri-oracle` to have keripy fold.

mod support;

#[path = "support/crate_kels.rs"]
mod crate_kels;

use std::fs;

use serde_json::{Value, json};

const FILE: &str = "kels.jsonl";

/// The vectors of the crate's KELs, without keripy's outcome, which the oracle adds.
fn written() -> Result<Vec<Value>, String> {
    let cases = crate_kels::cases().map_err(|e| e.to_string())?;
    cases
        .into_iter()
        .enumerate()
        .map(|(n, case)| {
            Ok(json!({
                "id": format!("crate/{n}"),
                "kind": "kel",
                "expect": "same",
                "source": {
                    "how": "crate",
                    "file": "crates/kerita/tests/support/crate_kels.rs",
                    "test": case.name,
                },
                "input": {
                    "steps": case.messages.iter().map(|m| Ok(json!({
                        "message": m.to_text().map_err(|e| e.to_string())?,
                        "validator": "remote",
                    }))).collect::<Result<Vec<_>, String>>()?,
                },
            }))
        })
        .collect()
}

/// The KELs the crate writes today are the ones committed for keripy, byte for byte: a change to
/// how the crate writes them shows here before the oracle folds them again.
#[test]
fn crate_kels_are_the_committed_ones() {
    let directory = support::corpus().join("crate");
    let fresh = written().unwrap();
    if std::env::var("KERITA_WRITE_CORPUS").as_deref() == Ok("1") {
        fs::create_dir_all(&directory).unwrap();
        let lines: String = fresh
            .iter()
            .flat_map(|v| [v.to_string(), "\n".to_owned()])
            .collect();
        fs::write(directory.join(FILE), lines).unwrap();
        return;
    }
    let committed = support::vectors("crate", FILE).unwrap();
    assert_eq!(
        committed.len(),
        fresh.len(),
        "KELs committed against KELs written"
    );
    for (ours, theirs) in fresh.iter().zip(&committed) {
        assert_eq!(ours["input"], theirs["input"], "{}: messages", ours["id"]);
    }
}
