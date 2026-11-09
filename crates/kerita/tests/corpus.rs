//! The corpus itself: every file holds the vectors its manifest counts, and every vector names
//! where it came from and what the crate is expected to answer.

mod support;

use std::collections::BTreeSet;
use std::fs;

use serde_json::Value;

const HOW: &[&str] = &["copied", "replayed", "generated", "cesr-rs", "crate"];
const EXPECT: &[&str] = &["same", "unsupported", "refused"];

/// Every corpus file holds as many vectors as its directory's manifest names, so a file emptied
/// or cut short fails here rather than passing as fewer cases.
#[test]
fn every_corpus_file_holds_the_count_its_manifest_names() {
    for directory in support::DIRECTORIES {
        let manifest = support::manifest(directory).unwrap();
        assert_ne!(manifest.len(), 0, "{directory}/MANIFEST lists no file");
        for (file, count) in &manifest {
            let vectors = support::vectors(directory, file).unwrap();
            assert_eq!(
                vectors.len(),
                *count,
                "{directory}/{file}: vectors read against the manifest"
            );
            assert_ne!(*count, 0, "{directory}/{file} holds no vector");
        }
    }
}

/// Every vector names its source — the keripy commit, how it was taken, the file and the test or
/// generator — and its expected outcome, and no two vectors of a file share an id.
#[test]
fn every_vector_names_its_source_and_expected_outcome() {
    for directory in support::DIRECTORIES {
        for file in support::manifest(directory).unwrap().keys() {
            let mut ids = BTreeSet::new();
            for vector in support::vectors(directory, file).unwrap() {
                let id = vector["id"].as_str().unwrap_or_default().to_owned();
                let at = format!("{directory}/{file}: vector {id:?}");
                assert!(!id.is_empty(), "{at} has no id");
                assert!(ids.insert(id.clone()), "{at} repeats an id");
                check_source(&vector["source"], &at);
                let expect = vector["expect"].as_str().unwrap_or_default();
                assert!(EXPECT.contains(&expect), "{at} expects {expect:?}");
                assert!(!vector["input"].is_null(), "{at} has no input");
                assert!(!vector["keripy"].is_null(), "{at} holds no keripy outcome");
            }
        }
    }
}

/// Every keripy test the oracle harvests passed at the pin: what a failing test built is missing
/// from the corpus, and `keripy/FAILED` names the test instead.
#[test]
fn every_harvested_keripy_test_passed_at_the_pin() {
    let failed = fs::read_to_string(support::corpus().join("keripy").join("FAILED")).unwrap();
    assert_eq!(failed, "", "keripy's tests that failed at the pin");
}

fn check_source(source: &Value, at: &str) {
    let commit = source["keripy"].as_str().unwrap_or_default();
    assert!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "{at} names no keripy commit: {source}"
    );
    let how = source["how"].as_str().unwrap_or_default();
    assert!(
        HOW.contains(&how),
        "{at} names no way it was taken: {source}"
    );
    for field in ["file", "test"] {
        assert!(
            source[field].as_str().is_some_and(|s| !s.is_empty()),
            "{at} names no {field}: {source}"
        );
    }
}
