//! Queries: what the engine answers, and within which pods and zones (the
//! mia-docs change `graph-query-engine`, capability `queries`). Following
//! relations, and nodes about no subject, are in `nodes.rs`.

mod common;

use std::sync::Arc;

use common::*;
use graph_query_engine::db::{zone, GraphCells, GraphDb, ZoneSource};
use graph_query_engine::record::RecordSource;
use graph_query_engine::{EntityQuery, FilterOp, Ontology, PageRequest, TypeId};
use serde_json::json;

fn zone_of(pod: &str, kind: &str, store: &Arc<Store>) -> ZoneSource {
    (
        pod.into(),
        kind.into(),
        Arc::clone(store) as Arc<dyn RecordSource>,
    )
}

fn meds_graph(pod: &str, n: usize) -> GraphDb {
    let db = GraphDb::in_memory(&Ontology::platform())
        .expect("the graph opens")
        .with_bulk_threshold(10);
    db.catch_up_zone(
        pod,
        MEDS,
        &Meds,
        &(0..n).map(med_record).collect::<Vec<_>>(),
    )
    .expect("the zone catches up");
    db
}

fn query(db: &GraphDb) -> EntityQuery {
    EntityQuery::new(
        db.shape(&TypeId::new(PET_MEDS))
            .expect("the type has a shape"),
    )
    .limit(1000)
}

fn scope(pod: &str, kind: &str) -> ([String; 1], [String; 1]) {
    ([pod.to_owned()], [zone(pod, kind)])
}

/// "Ginger" with notes and medication records, "Travel" with notes.
struct Pods {
    ginger_notes: Arc<Store>,
    ginger_meds: Arc<Store>,
    travel_notes: Arc<Store>,
}

fn pods() -> Pods {
    register_all();
    let about_ginger = json!({ "id": "ginger", "name": "Ginger" });
    Pods {
        ginger_notes: Store::new(vec![
            note("n1", "alice", 1, "vet on Friday"),
            note("n2", "bob", 3, "wet food at 07:30"),
            note("n3", "alice", 4, "hates the blue bowl"),
        ]),
        ginger_meds: Store::new(vec![
            record(
                "m1",
                "alice",
                2,
                json!({ "subject": about_ginger, "value": medication(1) }),
            ),
            record(
                "m2",
                "bob",
                5,
                json!({ "subject": about_ginger, "value": medication(2) }),
            ),
        ]),
        travel_notes: Store::new(vec![record(
            "n1",
            "alice",
            6,
            json!({ "text": "passport in the top drawer", "about": "passport" }),
        )]),
    }
}

// ------------------------------------------------- a handle's reach

/// A view of one pod reads that pod alone, though the graph holds two.
#[tokio::test]
async fn a_pod_reads_only_itself() {
    let p = pods();
    let alice = GraphCells::default().of("alice");
    let both = alice
        .view(vec![
            zone_of("ginger", NOTES, &p.ginger_notes),
            zone_of("travel", NOTES, &p.travel_notes),
        ])
        .unwrap();
    assert_eq!(both.nodes(None, None).await.unwrap().len(), 4);
    let ginger = alice
        .view(vec![zone_of("ginger", NOTES, &p.ginger_notes)])
        .unwrap();
    let read = ginger.nodes(None, None).await.unwrap();
    assert_eq!(read.len(), 3);
    assert!(read.iter().all(|n| n.pod_id == "ginger"));
}

/// A handle on one zone reads no node and no subject of another zone in the same graph.
#[tokio::test]
async fn a_handle_on_one_zone_reads_that_zone_alone() {
    let p = pods();
    let alice = GraphCells::default().of("alice");
    alice
        .pod_graph("ginger", NOTES, p.ginger_notes.clone())
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();
    let empty = Store::new(Vec::new());
    let meds = alice.pod_graph("ginger", MEDS, empty).unwrap();
    assert_eq!(
        meds.nodes(None, None).await.unwrap(),
        [],
        "the notes zone is in Alice's graph, and not this handle's"
    );
    assert_eq!(
        meds.subjects().await.unwrap(),
        [] as [graph_query_engine::SubjectView; 0]
    );
}

/// A view made for one zone reads none of another zone the graph holds, which a view made for both reads.
#[tokio::test]
async fn a_zone_a_view_was_not_made_for_is_not_read() {
    let p = pods();
    let cells = GraphCells::default();
    // Dan's graph holds the notes zone, and his view is made for the
    // medication records alone.
    cells
        .of("dan")
        .pod_graph("ginger", NOTES, p.ginger_notes.clone())
        .unwrap()
        .nodes(None, None)
        .await
        .unwrap();
    let dan = cells
        .of("dan")
        .view(vec![zone_of("ginger", MEDS, &p.ginger_meds)])
        .unwrap();
    assert!(dan
        .nodes(None, None)
        .await
        .unwrap()
        .iter()
        .all(|n| n.tool == MEDS));
    let alice = cells
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &p.ginger_notes),
            zone_of("ginger", MEDS, &p.ginger_meds),
        ])
        .unwrap();
    assert!(alice
        .nodes(None, None)
        .await
        .unwrap()
        .iter()
        .any(|n| n.value["content"] == "vet on Friday"));
}

/// A pod-tool reads another pod-tool's note through a view made for both zones.
#[tokio::test]
async fn a_pod_tool_reads_another_pod_tools_zone_through_a_view() {
    let p = pods();
    // The calendar's view of "Ginger", made for the notes zone as well.
    let calendar = Store::new(Vec::new());
    let view = GraphCells::default()
        .of("alice")
        .view(vec![
            zone_of("ginger", CALENDAR, &calendar),
            zone_of("ginger", NOTES, &p.ginger_notes),
        ])
        .unwrap();
    let read = view.nodes(None, None).await.unwrap();
    assert!(read
        .iter()
        .any(|n| n.tool == NOTES && n.value["content"] == "vet on Friday"));
}

/// Two pods with the same node ids each read, count and find their own.
#[test]
fn two_pods_with_the_same_node_ids_keep_them_apart() {
    let db = meds_graph("a", 40);
    let b: Vec<_> = (0..15)
        .map(|n| {
            let mut r = med_record(n);
            r.bytes = serde_json::to_vec(&json!({ "subject": { "id": format!("subject-{}", n % 5), "name": format!("Cat {} in B", n % 5) }, "value": medication(n) })).unwrap();
            r
        })
        .collect();
    db.catch_up_zone("b", MEDS, &Meds, &b).unwrap();
    let (pa, za) = scope("a", MEDS);
    let (pb, zb) = scope("b", MEDS);
    assert_eq!(db.find_in(&pa, &za, None, None).unwrap().len(), 40);
    assert_eq!(db.find_in(&pb, &zb, None, None).unwrap().len(), 15);
    assert!(db
        .subjects_in(&pb, &zb)
        .unwrap()
        .iter()
        .all(|s| s.display_name.ends_with("in B")));
    let clavamox = query(&db)
        .filter(
            &["medication", "brand_name"],
            FilterOp::Eq,
            json!("Clavamox"),
        )
        .unwrap();
    assert_eq!(
        (
            db.count_in(&pa, &za, &clavamox).unwrap(),
            db.count_in(&pb, &zb, &clavamox).unwrap()
        ),
        (20, 8)
    );
    assert!(
        db.get_in(&pb, &zb, "claim020").unwrap().is_none(),
        "b has no claim020, whatever a holds"
    );
}

// ------------------------------------------------- finding and counting

/// Finds and counts by a field's value, a list holding a value, a date at least a given one, a subject and a limit return exactly what matches.
#[test]
fn filters_subjects_and_limits_are_applied_in_the_graph() {
    let db = meds_graph("p", 40);
    let (pods, zones) = scope("p", MEDS);
    let clavamox = query(&db)
        .filter(
            &["medication", "brand_name"],
            FilterOp::Eq,
            json!("Clavamox"),
        )
        .unwrap();
    assert_eq!(db.count_in(&pods, &zones, &clavamox).unwrap(), 20);
    assert!(db
        .find_in(&pods, &zones, Some(&clavamox), None)
        .unwrap()
        .iter()
        .all(|n| n.value["medication"][0]["brand_name"] == "Clavamox"));

    let ingredient = query(&db)
        .filter(
            &["medication", "active_ingredient"],
            FilterOp::Contains,
            json!("Ingredient 7"),
        )
        .unwrap();
    let found = db.find_in(&pods, &zones, Some(&ingredient), None).unwrap();
    assert_eq!(
        found.iter().map(|n| n.node_id.as_str()).collect::<Vec<_>>(),
        ["claim007"]
    );

    let late = query(&db)
        .filter(
            &["medication", "administration", "start_date"],
            FilterOp::Gte,
            json!("2026-10-25"),
        )
        .unwrap();
    let late = db.find_in(&pods, &zones, Some(&late), None).unwrap();
    assert_ne!(late, [] as [graph_query_engine::NodeView; 0]);
    assert!(late
        .iter()
        .all(|n| n.value["medication"][0]["administration"]["start_date"]
            .as_str()
            .unwrap()
            >= "2026-10-25"));

    assert_eq!(
        db.find_in(&pods, &zones, None, Some(&["subject-3".to_owned()]))
            .unwrap()
            .len(),
        8
    );
    let first_three = db
        .find_in(&pods, &zones, Some(&query(&db).limit(3)), None)
        .unwrap();
    assert_eq!(
        first_three
            .iter()
            .map(|n| n.node_id.as_str())
            .collect::<Vec<_>>(),
        ["claim000", "claim001", "claim002"],
        "oldest first, no more than the limit"
    );
}

/// The types present are counted per pod within the view, and a view of one zone lists that zone's types alone.
#[tokio::test]
async fn the_types_present_are_counted_per_pod_within_the_view() {
    let p = pods();
    let cells = GraphCells::default();
    let alice = cells
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &p.ginger_notes),
            zone_of("ginger", MEDS, &p.ginger_meds),
            zone_of("travel", NOTES, &p.travel_notes),
        ])
        .unwrap();
    let counted: Vec<(String, Vec<(String, usize)>)> = alice
        .present_types()
        .await
        .unwrap()
        .into_iter()
        .map(|(t, counts)| (t.name, counts))
        .collect();
    assert_eq!(
        counted,
        [
            (
                "Note".to_owned(),
                vec![("ginger".to_owned(), 3), ("travel".to_owned(), 1)]
            ),
            (
                "PetMedicationRecord".to_owned(),
                vec![("ginger".to_owned(), 2)]
            ),
        ]
    );
    let dan = cells
        .of("dan")
        .view(vec![zone_of("ginger", MEDS, &p.ginger_meds)])
        .unwrap();
    let his: Vec<(String, Vec<(String, usize)>)> = dan
        .present_types()
        .await
        .unwrap()
        .into_iter()
        .map(|(t, counts)| (t.name, counts))
        .collect();
    assert_eq!(
        his,
        [(
            "PetMedicationRecord".to_owned(),
            vec![("ginger".to_owned(), 2)]
        )]
    );
}

/// A subject carries its pod, its claim count and its type names in the order of their oldest claim.
#[tokio::test]
async fn a_subject_carries_its_claim_count_and_its_types() {
    let p = pods();
    let view = GraphCells::default()
        .of("alice")
        .view(vec![
            zone_of("ginger", NOTES, &p.ginger_notes),
            zone_of("ginger", MEDS, &p.ginger_meds),
        ])
        .unwrap();
    let subjects = view.subjects().await.unwrap();
    assert_eq!(subjects.len(), 1);
    let ginger = &subjects[0];
    assert_eq!(
        (
            ginger.subject_id.as_str(),
            ginger.pod_id.as_str(),
            ginger.claims
        ),
        ("ginger", "ginger", 5)
    );
    assert_eq!(
        ginger.types,
        ["Note", "PetMedicationRecord"],
        "each type once, by its oldest claim"
    );
}

/// A list of 50 is read 20 at a time, newest first, each page with the total.
#[tokio::test]
async fn a_list_of_50_is_read_20_at_a_time_newest_first() {
    register_all();
    let notes = Store::new(
        (0..50)
            .map(|i| note(&format!("n{i:02}"), "alice", i, &format!("note {i:02}")))
            .collect(),
    );
    let view = GraphCells::default()
        .of("alice")
        .view(vec![zone_of("p", NOTES, &notes)])
        .unwrap();
    let page = |offset| PageRequest {
        tool: Some(NOTES.into()),
        newest_first: true,
        offset,
        limit: 20,
        ..PageRequest::default()
    };
    let content =
        |n: &graph_query_engine::NodeView| n.value["content"].as_str().unwrap().to_owned();
    let first = view.page(&page(0)).await.unwrap();
    assert_eq!((first.total, first.nodes.len()), (50, 20));
    assert_eq!(
        (content(&first.nodes[0]), content(&first.nodes[19])),
        ("note 49".to_owned(), "note 30".to_owned())
    );
    let second = view.page(&page(20)).await.unwrap();
    assert_eq!((second.total, second.nodes.len()), (50, 20));
    assert_eq!(content(&second.nodes[0]), "note 29");
}

// ------------------------------------------------- where it is from

/// An event's time comes back as it went in, with its pod-tool and its record's device.
#[tokio::test]
async fn an_event_with_a_time_comes_back_as_it_went_in() {
    register_all();
    let value = json!({ "title": "vet", "starts_at": "2026-11-21T10:00:00Z", "place": "Linden Street clinic" });
    let calendar = Store::new(vec![record("e1", "alice", 1, value.clone())]);
    let view = GraphCells::default()
        .of("alice")
        .view(vec![zone_of("ginger", CALENDAR, &calendar)])
        .unwrap();
    let read = view.nodes(None, None).await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].value["title"], "vet");
    assert_eq!(
        read[0].value["starts_at"], "2026-11-21T10:00:00Z",
        "a time comes back as it went in"
    );
    assert_eq!(
        (read[0].tool.as_str(), read[0].device.as_deref()),
        (CALENDAR, Some("alice-phone")),
        "its device is the record's"
    );
}

/// Each of 40 claims reads back with its value tree, member, time, subject and root value id.
#[test]
fn a_node_reads_back_exactly_as_it_was_written() {
    let db = meds_graph("p", 40);
    let (pods, zones) = scope("p", MEDS);
    for (n, node) in db
        .find_in(&pods, &zones, None, None)
        .unwrap()
        .into_iter()
        .enumerate()
    {
        let written = med_record(n);
        assert_eq!(node.value, medication(n), "the value tree intact");
        assert_eq!(node.member, written.author);
        assert_eq!(
            node.at,
            written
                .at
                .format(time::macros::format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z"
                ))
                .unwrap()
        );
        assert_eq!(
            node.subject_id(),
            Some(format!("subject-{}", n % 5).as_str())
        );
        assert_eq!(node.root_value_id, format!("claim{n:03}/value"));
        assert_eq!(node.pod_id, "p");
    }
}
