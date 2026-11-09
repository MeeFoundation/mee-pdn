//! Reading the corpus: its directories, their manifests and the vectors of each file. The format
//! is described in `oracle/corpus.py`, which writes it.
// Each test binary uses part of this module.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The corpus directories, each with its own `MANIFEST`.
pub const DIRECTORIES: &[&str] = &["keripy", "crate"];

pub fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// `{file name: vector count}` as a directory's `MANIFEST` lists them.
pub fn manifest(directory: &str) -> Result<BTreeMap<String, usize>, String> {
    let path = corpus().join(directory).join("MANIFEST");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .map(|line| {
            let (name, count) = line
                .split_once(' ')
                .ok_or_else(|| format!("{}: malformed line {line:?}", path.display()))?;
            let count = count
                .parse()
                .map_err(|e| format!("{}: count of {name}: {e}", path.display()))?;
            Ok((name.to_owned(), count))
        })
        .collect()
}

/// The vectors of one corpus file, in order.
pub fn vectors(directory: &str, file: &str) -> Result<Vec<Value>, String> {
    let path = corpus().join(directory).join(file);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .map(|(n, line)| {
            serde_json::from_str(line)
                .map_err(|e| format!("{}:{}: {e}", path.display(), n.saturating_add(1)))
        })
        .collect()
}
