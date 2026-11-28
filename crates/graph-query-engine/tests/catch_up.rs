//! Catch-up: how the graph is brought up to date with the records (the
//! mia-docs change `graph-query-engine`, capability `catch-up`). Scenarios
//! with nodes and relations of several kinds are in `nodes.rs`.

mod common;

use std::sync::Arc;

use common::*;
use graph_query_engine::db::{zone, Applied, GraphCells, GraphDb};
use graph_query_engine::Ontology;
use serde_json::json;

fn graph(bulk_from: usize) -> GraphDb {
    GraphDb::in_memory(&Ontology::platform())
        .expect("the graph opens")
        .of_identity(Some("alice".into()))
        .with_bulk_threshold(bulk_from)
}

fn notes(n: i64) -> Vec<graph_query_engine::record::Record> {
    (0..n)
        .map(|i| note(&format!("n{i:02}"), "alice", i, &format!("note {i:02}")))
        .collect()
}

/// One identity's two pods share one graph, and another identity reads from a graph of its own.
#[tokio::test]
async fn one_identitys_pods_share_one_graph() {
    register_all();
    let cells = GraphCells::default();
    let ginger = Store::new(vec![note("n1", "alice", 1, "vet on Friday")]);
    let travel = Store::new(vec![note("n1", "alice", 2, "passport in the top drawer")]);
    let alice = cells.of("alice");
    alice
        .pod_graph("ginger", NOTES, ginger.clone())
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();
    alice
        .pod_graph("travel", NOTES, travel)
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();
    let bob = cells.of("bob");
    bob.pod_graph("ginger", NOTES, ginger)
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();

    assert_eq!(
        alice.db().unwrap().pods(),
        ["ginger", "travel"],
        "one graph holds both of Alice's pods"
    );
    assert_eq!(bob.db().unwrap().pods(), ["ginger"]);
    assert!(
        !Arc::ptr_eq(&alice.db().unwrap(), &bob.db().unwrap()),
        "Bob reads from a graph of his own"
    );
    assert!(
        Arc::ptr_eq(&alice.db().unwrap(), &cells.of("alice").db().unwrap()),
        "and Alice from one, however often asked"
    );
}

/// A graph whose handles were made for no zone of a pod holds nothing of it.
#[tokio::test]
async fn nothing_is_built_of_a_zone_no_handle_was_made_for() {
    register_all();
    let cells = GraphCells::default();
    let ginger = Store::new(vec![note("n1", "alice", 1, "vet on Friday")]);
    cells
        .of("alice")
        .pod_graph("ginger", NOTES, ginger)
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();
    assert_eq!(
        cells.of("carol").db().unwrap().pods(),
        [] as [std::string::String; 0]
    );
}

/// A record another member writes is read on the handle's next read.
#[tokio::test]
async fn a_record_written_by_another_member_is_read_on_the_next_read() {
    register_all();
    let store = Store::new(vec![note("n1", "alice", 1, "vet on Friday")]);
    let handle = GraphCells::default()
        .of("alice")
        .pod_graph("ginger", NOTES, store.clone())
        .unwrap();
    assert_eq!(handle.nodes(None, None).await.unwrap().len(), 1);

    store.add(note("n2", "bob", 2, "Ginger hates the blue bowl"));
    let read = handle.nodes(None, None).await.unwrap();
    assert_eq!(read.len(), 2, "the next read applies Bob's record first");
    assert_eq!(read[1].member, "bob");
}

/// A zone at the version it was caught up with is not read again, and is once the version moves; a store with no version is read every time.
#[tokio::test]
async fn an_unchanged_zone_is_not_read_again_and_is_read_once_its_version_moves() {
    register_all();
    let versioned = Store::versioned(vec![note("n1", "alice", 1, "vet on Friday")]);
    let unversioned = Store::new(vec![note("n1", "alice", 1, "vet on Friday")]);
    let cells = GraphCells::default();
    let with_version = cells
        .of("alice")
        .pod_graph("ginger", NOTES, versioned.clone())
        .unwrap();
    let without = cells
        .of("alice")
        .pod_graph("travel", NOTES, unversioned.clone())
        .unwrap();

    for _ in 0..2 {
        with_version.nodes(None, None).await.unwrap();
        without.nodes(None, None).await.unwrap();
    }
    assert_eq!(
        versioned.reads(),
        1,
        "a store at the same version is not read again"
    );
    assert_eq!(
        unversioned.reads(),
        2,
        "a store with no version is read every time"
    );

    versioned.add(note("n2", "bob", 2, "Ginger hates the blue bowl"));
    assert_eq!(with_version.nodes(None, None).await.unwrap().len(), 2);
    assert_eq!(versioned.reads(), 2, "read again once its version moved");
}

/// Of 12 records, one whose value does not fit and one of an unknown type are skipped, the other ten load, and neither is tried again.
#[test]
fn two_nodes_that_do_not_fit_are_skipped_and_the_other_ten_load() {
    let mut records: Vec<_> = (0..12).map(med_record).collect();
    records[5] = record(
        "claim005",
        "bob",
        5,
        json!({ "subject": { "id": "subject-0", "name": "Cat 0" }, "value": { "medication": "not a list" } }),
    );
    records[6] = record(
        "claim006",
        "alice",
        6,
        json!({ "subject": { "id": "subject-1", "name": "Cat 1" }, "type": NO_SUCH_TYPE, "value": medication(6) }),
    );
    let db = graph(10);
    assert_eq!(
        db.catch_up_zone("p", MEDS, &Meds, &records).unwrap(),
        Applied {
            bulk: 10,
            skipped: 2,
            ..Applied::default()
        }
    );
    assert_eq!(
        db.find_in(&["p".into()], &[zone("p", MEDS)], None, None)
            .unwrap()
            .len(),
        10
    );
    assert_eq!(
        db.catch_up_zone("p", MEDS, &Meds, &records).unwrap(),
        Applied::default(),
        "neither is tried again"
    );
}

/// A bulk load that fails after its subjects and nodes have committed leaves nothing of what it wrote: the keys applied one at a time instead give the graph applying them key by key gives, subjects and their types included.
#[test]
fn a_bulk_load_that_fails_leaves_nothing_and_its_keys_are_applied_one_at_a_time() {
    let records: Vec<_> = (0..40).map(med_record).collect();
    let failing = graph(10).with_bulk_failing_at("Link");
    let applied = failing.catch_up_zone("p", MEDS, &Meds, &records).unwrap();
    assert_eq!(
        (applied.bulk, applied.appended, applied.skipped),
        (0, 40, 0),
        "every key applied one at a time instead"
    );

    let key_by_key = graph(1_000);
    key_by_key
        .catch_up_zone("p", MEDS, &Meds, &records)
        .unwrap();
    let (pods, zones) = (["p".to_owned()], [zone("p", MEDS)]);
    let read = failing.find_in(&pods, &zones, None, None).unwrap();
    assert_eq!(read.len(), 40, "each claim once");
    assert_eq!(read, key_by_key.find_in(&pods, &zones, None, None).unwrap());
    let subjects = failing.subjects_in(&pods, &zones).unwrap();
    assert_eq!(subjects.len(), 5);
    assert!(
        subjects
            .iter()
            .all(|s| s.type_id.as_deref() == Some(SUBJECT)),
        "every subject keeps its type: {subjects:?}"
    );
    assert_eq!(subjects, key_by_key.subjects_in(&pods, &zones).unwrap());
}

/// A key added after a bulk load is applied on its own.
#[test]
fn a_new_key_after_the_first_load_is_applied_on_its_own() {
    let db = graph(10);
    let mut records = notes(40);
    assert_eq!(
        db.catch_up_zone("p", NOTES, &Notes, &records).unwrap(),
        Applied {
            bulk: 40,
            ..Applied::default()
        }
    );
    records.push(note("n40", "bob", 40, "note 40"));
    assert_eq!(
        db.catch_up_zone("p", NOTES, &Notes, &records).unwrap(),
        Applied {
            appended: 1,
            ..Applied::default()
        }
    );
    assert_eq!(
        db.find_in(&["p".into()], &[zone("p", NOTES)], None, None)
            .unwrap()
            .len(),
        41
    );
}

/// A backlog of 40 loads in bulk and reads back oldest first, each value as written.
#[test]
fn a_pod_new_to_the_graph_loads_its_backlog_in_bulk_and_reads_back_as_written() {
    let db = graph(10);
    let records: Vec<_> = (0..40).map(med_record).collect();
    assert_eq!(
        db.catch_up_zone("p", MEDS, &Meds, &records).unwrap().bulk,
        40
    );
    let read = db
        .find_in(&["p".into()], &[zone("p", MEDS)], None, None)
        .unwrap();
    assert_eq!(read.len(), 40);
    for (n, node) in read.iter().enumerate() {
        assert_eq!(node.node_id, format!("claim{n:03}"), "oldest first");
        assert_eq!(node.value, medication(n), "each value as it was written");
    }
}
