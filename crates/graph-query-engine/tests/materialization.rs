//! The materialization interface: how a pod-tool exposes its data to the
//! engine, and what the engine does with it (the mia-docs change
//! `graph-query-engine`, capability `materialization-interface`). Scenarios
//! of nodes and relations of several kinds are in `nodes.rs`.

mod common;

use std::sync::Arc;

use common::*;
use graph_query_engine::db::{zone, GraphCells, GraphDb, ZoneSource};
use graph_query_engine::materialize::{
    newest, register, Materialization, Materialized, Node, Subject, ToolOntology, ToolType,
};
use graph_query_engine::record::{Record, RecordSource};
use graph_query_engine::{EntityQuery, Ontology, TypeId};
use serde_json::json;

fn zone_of(pod: &str, kind: &str, store: &Arc<Store>) -> ZoneSource {
    (
        pod.into(),
        kind.into(),
        Arc::clone(store) as Arc<dyn RecordSource>,
    )
}

/// Flea-treatment notes: a pod-tool the engine was not built with.
struct FleaNotes;

impl Materialization for FleaNotes {
    fn kind(&self) -> &'static str {
        "flea-notes"
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "flea-notes".into(),
            types: vec![ToolType {
                type_id: NOTE.into(),
                extends: None,
                embeddable: true,
            }],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            Some(
                Node::new(format!("flea-{key}"), NOTE, json!({ "content": r.text() })).about(
                    Subject {
                        id: "ginger".into(),
                        name: "Ginger".into(),
                        type_id: SUBJECT.into(),
                    },
                ),
            )
        })
    }
}

/// A pod-tool registered after the engine started is read beside the others.
#[tokio::test]
async fn a_pod_tool_registered_while_the_app_runs_is_read_like_any_other() {
    register_all();
    register(Arc::new(FleaNotes));
    let notes = Store::new(vec![note("n1", "bob", 1, "vet on Friday")]);
    let flea = Store::new(vec![Record {
        bytes: b"Ginger's flea treatment is due on the 3rd".to_vec(),
        ..note("n1", "alice", 2, "")
    }]);
    let view = GraphCells::default()
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &notes),
            zone_of("ginger", "flea-notes", &flea),
        ])
        .unwrap();
    let read = view.nodes(None, None).await.unwrap();
    let contents: Vec<&str> = read
        .iter()
        .map(|n| n.value["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        ["vet on Friday", "Ginger's flea treatment is due on the 3rd"]
    );
    assert_eq!(read[1].tool, "flea-notes");
}

/// A view tells the tool ontologies of the zones it was made for, and no other.
#[tokio::test]
async fn a_view_tells_the_tool_ontologies_of_its_zones_and_no_other() {
    register_all();
    let (notes, meds) = (Store::new(Vec::new()), Store::new(Vec::new()));
    let cells = GraphCells::default();
    let alice = cells
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &notes),
            zone_of("ginger", MEDS, &meds),
        ])
        .unwrap();
    let dan = cells
        .of("dan")
        .view(vec![zone_of("ginger", MEDS, &meds)])
        .unwrap();
    let kinds = |o: Vec<ToolOntology>| o.into_iter().map(|o| o.kind).collect::<Vec<_>>();
    assert_eq!(kinds(alice.tool_ontologies()), [NOTES, MEDS]);
    assert_eq!(kinds(dan.tool_ontologies()), [MEDS]);
    assert!(
        alice.tool_ontologies()[1]
            .types
            .iter()
            .any(|t| t.type_id == PET_MEDS),
        "with its types"
    );
}

/// A pod-tool with no materialization contributes nothing, and its records are not read.
#[tokio::test]
async fn a_pod_tool_that_registers_no_materialization_is_not_read() {
    register_all();
    let notes = Store::new(vec![note("n1", "bob", 1, "vet on Friday")]);
    let unregistered = Store::new(vec![note(
        "x1",
        "alice",
        2,
        "written by a pod-tool with no materialization",
    )]);
    let view = GraphCells::default()
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &notes),
            zone_of("ginger", "no-materialization", &unregistered),
        ])
        .unwrap();
    let read = view.nodes(None, None).await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].tool, NOTES);
    assert_eq!(unregistered.reads(), 0, "its records are not even read");
    assert!(view
        .tool_ontologies()
        .iter()
        .all(|o| o.kind != "no-materialization"));
}

/// With no materialization for a zone, the engine still gives shapes, and finds and counts no node.
#[tokio::test]
async fn the_engine_answers_with_no_pod_tool_registered() {
    // A zone whose pod-tool registered nothing: the engine loads its
    // registry and answers, with nothing to read.
    let records = Store::new(vec![med_record(0)]);
    let handle = GraphCells::default()
        .of("alice")
        .pod_graph("ginger", "nothing-registered", records)
        .unwrap();
    let shape = handle.get_shape(&TypeId::new(PET_MEDS)).await.unwrap();
    assert_eq!(shape.type_info.name, "PetMedicationRecord");
    assert_eq!(
        handle.nodes(None, None).await.unwrap(),
        [] as [graph_query_engine::NodeView; 0]
    );
    assert_eq!(handle.count(&EntityQuery::new(shape)).await.unwrap(), 0);
}

/// Two members' graphs built apart from the same records read the same, as does a graph built again.
#[test]
fn two_members_graphs_built_apart_read_the_same_and_so_does_a_graph_built_again() {
    let records: Vec<_> = vec![
        note("n1", "bob", 1, "vet on Friday"),
        note("n2", "alice", 2, "I'll take her"),
    ];
    let graph = |who: &str| {
        let db = GraphDb::in_memory(&Ontology::platform())
            .unwrap()
            .of_identity(Some(who.into()));
        db.catch_up_zone("ginger", NOTES, &Notes, &records).unwrap();
        db.find_in(&["ginger".into()], &[zone("ginger", NOTES)], None, None)
            .unwrap()
    };
    let (alice, bob, again) = (graph("alice"), graph("bob"), graph("alice"));
    assert_eq!(alice.len(), 2);
    assert_eq!(
        alice, bob,
        "the same nodes, ids, pod-tool, type, value and member in both"
    );
    assert_eq!(
        alice, again,
        "a graph built again from the records reads the same"
    );
}

/// A note pod-tool whose notes carry a mark of which registration made them.
struct Marked(&'static str);

impl Materialization for Marked {
    fn kind(&self) -> &'static str {
        "marked-notes"
    }
    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "marked-notes".into(),
            types: vec![ToolType {
                type_id: NOTE.into(),
                extends: None,
                embeddable: true,
            }],
            relations: Vec::new(),
        }
    }
    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        let mark = self.0;
        newest(records, |r| {
            Some(
                Node::new(
                    key,
                    NOTE,
                    json!({ "content": format!("{mark}: {}", r.text()) }),
                )
                .about(Subject {
                    id: "ginger".into(),
                    name: "Ginger".into(),
                    type_id: SUBJECT.into(),
                }),
            )
        })
    }
}

/// A materialization registered for a kind already registered takes effect at the next read, the store's version unchanged.
#[tokio::test]
async fn a_materialization_registered_for_a_kind_already_registered_replaces_it_from_the_next_catch_up(
) {
    register(Arc::new(Marked("first")));
    // A store that keeps a version, which does not move: the records are the
    // same, and the zone is still made again.
    let store = Store::versioned(vec![Record {
        bytes: b"vet on Friday".to_vec(),
        ..note("n1", "alice", 1, "")
    }]);
    let handle = GraphCells::default()
        .of("alice")
        .pod_graph("ginger", "marked-notes", store)
        .unwrap();
    assert_eq!(
        handle.nodes(None, None).await.unwrap()[0].value["content"],
        "first: vet on Friday"
    );
    register(Arc::new(Marked("second")));
    assert_eq!(
        handle.nodes(None, None).await.unwrap()[0].value["content"],
        "second: vet on Friday"
    );
}
