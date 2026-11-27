//! The vector index: `LadybugDB`'s vector extension, linked into the engine,
//! made and searched with nothing downloaded or installed at run time.

use graph_query_engine::vector::{Entry, VectorIndex};

fn entry(id: &str, pod: &str, text: &str) -> Entry {
    Entry {
        id: id.into(),
        pod: pod.into(),
        kind: "fact".into(),
        reference: id.into(),
        text: text.into(),
    }
}

fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0.01f32; 8];
    v[i] = 1.0;
    v
}

/// An index is made and searched with nothing installed, and returns the nearest entries, nearest first.
#[test]
fn an_hnsw_index_is_made_and_queried_with_nothing_installed() {
    let index = VectorIndex::new(
        vec![
            entry("passport", "Travel", "Alice's passport expires 2027-09-01"),
            entry("licence", "Travel", "Alice's licence expires 2026-12-15"),
            entry("ginger", "Ginger", "Ginger takes amoxicillin"),
        ],
        &[[0.9f32, 0.1, 0.0], [0.8, 0.3, 0.1], [0.0, 0.2, 0.9]],
        3,
    )
    .unwrap();
    let found = index.nearest(&[1.0, 0.1, 0.0], 2).unwrap();
    let ids: Vec<&str> = found.iter().map(|(e, _)| e.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["passport", "licence"],
        "the two nearest, nearest first"
    );
    assert_eq!(
        found[0].0,
        entry("passport", "Travel", "Alice's passport expires 2027-09-01"),
        "returned as given"
    );
    assert!(found[0].1 <= found[1].1);
}

/// An owner adds what it derives to the index it has, rather than building
/// the index again: an entry added after the index was made is found.
#[test]
fn a_chunk_added_after_the_index_was_made_is_found() {
    let index = VectorIndex::new(
        (0..200)
            .map(|i| entry(&format!("old{i}"), "p", "old"))
            .collect(),
        &(0..200).map(|i| unit(i % 7)).collect::<Vec<_>>(),
        8,
    )
    .unwrap();
    index
        .add(vec![entry("new", "p", "new")], &[unit(7)])
        .unwrap();
    assert_eq!(index.nearest(&unit(7), 1).unwrap()[0].0.id, "new");
    assert_eq!(index.len(), 201);

    let empty = VectorIndex::new::<Vec<f32>>(Vec::new(), &[], 8).unwrap();
    empty
        .add(vec![entry("first", "p", "first")], &[unit(3)])
        .unwrap();
    assert_eq!(
        empty.nearest(&unit(3), 1).unwrap()[0].0.id,
        "first",
        "an index made empty takes its first entries"
    );
}

/// An index with no entries finds nothing.
#[test]
fn an_empty_index_finds_nothing() {
    let empty = VectorIndex::new::<Vec<f32>>(Vec::new(), &[], 8).unwrap();
    assert_eq!(
        empty.nearest(&unit(0), 5).unwrap(),
        [] as [(graph_query_engine::vector::Entry, f64); 0]
    );
}

/// An index leaves no files behind once dropped.
#[test]
fn an_index_leaves_no_files_behind() {
    let index = VectorIndex::new(vec![entry("a", "p", "a")], &[unit(1)], 8).unwrap();
    let path = index.path().to_path_buf();
    assert!(path.exists(), "the index keeps its files while it lives");
    assert_eq!(index.nearest(&unit(1), 1).unwrap().len(), 1);
    drop(index);
    assert!(
        !path.exists(),
        "nothing of the index is left once its owner lets go of it"
    );
}

/// What an index of `n` vectors costs in memory: run by hand,
/// `cargo test --release --test vector_index index_memory -- --ignored --nocapture`.
#[test]
#[ignore]
fn index_memory() {
    let rss = || {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            / 1024
    };
    for n in [500usize, 5000] {
        let before = rss();
        let entries: Vec<Entry> = (0..n)
            .map(|i| {
                entry(
                    &format!("c{i}"),
                    "p",
                    &format!("text number {i} about something"),
                )
            })
            .collect();
        let vectors: Vec<Vec<f32>> = (0..n)
            .map(|i| {
                (0..768)
                    .map(|j| ((i * 31 + j * 17) % 97) as f32 / 97.0)
                    .collect()
            })
            .collect();
        let index = VectorIndex::new(entries, &vectors, 768).unwrap();
        let built = rss();
        index
            .add(vec![entry("x", "p", "x")], &[vec![0.5f32; 768]])
            .unwrap();
        println!(
            "{n} vectors: {} MB before, {} MB built (+{}), {} MB after one add",
            before,
            built,
            built - before,
            rss()
        );
        drop(index);
        println!("  {} MB dropped", rss());
    }
}
