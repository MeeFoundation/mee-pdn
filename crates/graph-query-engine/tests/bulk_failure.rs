//! A bulk load that cannot stage its files (catch-up; design, operating
//! conditions: the disk fills). Alone in its test binary: it points the
//! temporary directory elsewhere, which no other test may see.

mod common;

use common::*;
use graph_query_engine::db::{zone, GraphDb};
use graph_query_engine::Ontology;

/// A bulk load that cannot stage its files leaves nothing behind, and every key is applied one at a time instead.
#[test]
fn a_bulk_load_that_cannot_stage_its_files_leaves_nothing_and_the_keys_are_applied_one_at_a_time() {
    let db = GraphDb::in_memory(&Ontology::platform())
        .unwrap()
        .with_bulk_threshold(10);
    let records: Vec<_> = (0..40)
        .map(|i| note(&format!("n{i:02}"), "alice", i, &format!("note {i:02}")))
        .collect();

    // A bulk load stages its rows as files in the temporary directory. A
    // temporary directory that is a file cannot hold them, as a full disk
    // cannot.
    let blocker = std::env::temp_dir().join(format!("gqe-blocked-{}", std::process::id()));
    std::fs::write(&blocker, b"").unwrap();
    std::env::set_var("TMPDIR", &blocker);
    let applied = db.catch_up_zone("p", NOTES, &Notes, &records);
    std::env::remove_var("TMPDIR");
    std::fs::remove_file(&blocker).unwrap();

    let applied = applied.unwrap();
    assert_eq!(
        (applied.bulk, applied.appended),
        (0, 40),
        "every key applied one at a time instead"
    );
    let read = db
        .find_in(&["p".into()], &[zone("p", NOTES)], None, None)
        .unwrap();
    assert_eq!(
        read.len(),
        40,
        "each note once: the failed load left nothing behind"
    );
    assert_eq!(read[0].value["content"], "note 00");
}
