//! Nodes and relations: what a pod-tool's records stand for (materialization
//! interface), how they reach the graph (catch-up) and how they are read
//! (queries). Each test is a scenario of the change `graph-query-engine`.

use chrono::{TimeZone, Utc};
use graph_query_engine::db::{zone, GraphDb};
use graph_query_engine::materialize::{
    newest, Materialization, Materialized, Node, Relation, Subject, ToolOntology, ToolRelation,
    ToolType,
};
use graph_query_engine::record::Record;
use graph_query_engine::{Direction, NodeView, Ontology};
use serde_json::{json, Value};

const POD: &str = "ginger";
const MESSAGE: &str = "app:Message:v1";
const NOTE: &str = "app:Note:v1";
const TEXT: &str = "app:UnstructuredText:v1";
const ATTACHMENT: &str = "app:Attachment:v1";

fn record(key: &str, author: &str, second: i64, bytes: Value) -> Record {
    Record {
        id: format!("{key}.{second}.{author}"),
        key: key.into(),
        author: author.into(),
        at: Utc.timestamp_opt(1_790_000_000 + second, 0).unwrap(),
        device: Some(format!("{author}-phone")),
        bytes: serde_json::to_vec(&bytes).expect("a JSON value serializes"),
    }
}

fn body(record: &Record) -> Value {
    serde_json::from_slice(&record.bytes).expect("a record of these tests is JSON")
}

fn tool_type(type_id: &str) -> ToolType {
    ToolType {
        type_id: type_id.into(),
        extends: None,
        embeddable: true,
    }
}

fn graph(identity: &str) -> GraphDb {
    GraphDb::in_memory(&Ontology::platform())
        .expect("the graph opens")
        .of_identity(Some(identity.into()))
}

fn all(db: &GraphDb, tools: &[&str]) -> Vec<NodeView> {
    let zones: Vec<String> = tools.iter().map(|t| zone(POD, t)).collect();
    db.find_in(&[POD.into()], &zones, None, None)
        .expect("the zone reads")
}

fn ids(nodes: &[NodeView]) -> Vec<String> {
    nodes.iter().map(|n| n.node_id.clone()).collect()
}

fn related(
    db: &GraphDb,
    tools: &[&str],
    tool: &str,
    id: &str,
    name: &str,
    direction: Direction,
) -> Vec<String> {
    let zones: Vec<String> = tools.iter().map(|t| zone(POD, t)).collect();
    ids(&db
        .related_in(&[POD.into()], &zones, Some(tool), id, name, direction)
        .expect("the relation is followed"))
}

/// A chat: each key one message, the newest record standing, following the
/// message it names, for the members it names where it names any.
struct Chat;

impl Materialization for Chat {
    fn kind(&self) -> &'static str {
        "chat"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "chat".into(),
            types: vec![tool_type(MESSAGE)],
            relations: vec![ToolRelation {
                name: "follows".into(),
                from_type: MESSAGE.into(),
                to_type: MESSAGE.into(),
            }],
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        let Some(last) = records.last() else {
            return Materialized::nothing();
        };
        let b = body(last);
        let readers: Vec<String> = b["to"]
            .as_array()
            .map(|to| {
                to.iter()
                    .filter_map(|m| m.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let node = Node::new(key, MESSAGE, json!({ "content": b["text"] })).for_members(readers);
        let mut out = Materialized::node(node);
        if let Some(before) = b["follows"].as_str() {
            out.relations.push(Relation::new("follows", key, before));
        }
        out
    }
}

/// Notes: the newest record under a key, a claim about the subject it names.
struct Notes;

impl Materialization for Notes {
    fn kind(&self) -> &'static str {
        "notes"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "notes".into(),
            types: vec![tool_type(NOTE)],
            relations: Vec::new(),
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            let b = body(r);
            let text = b["text"].as_str()?;
            Some(
                Node::new(key, NOTE, json!({ "content": text })).about(Subject {
                    id: "ginger".into(),
                    name: "Ginger".into(),
                    type_id: "mee:Subject:v1".into(),
                }),
            )
        })
    }
}

/// A document every member may edit: every operation under a key, merged.
/// It takes its member and time from the record that set its title.
struct Docs;

impl Materialization for Docs {
    fn kind(&self) -> &'static str {
        "docs"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "docs".into(),
            types: vec![tool_type(NOTE)],
            relations: Vec::new(),
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        let (mut title, mut lines, mut titled_by) = (String::new(), Vec::new(), None);
        for r in records {
            let b = body(r);
            if let Some(t) = b["title"].as_str() {
                title = t.to_owned();
                titled_by = Some(r.id.clone());
            }
            if let Some(line) = b["add"].as_str() {
                lines.push(line.to_owned());
            }
        }
        let mut node = Node::new(
            key,
            NOTE,
            json!({ "title": title, "content": lines.join("; ") }),
        );
        if let Some(id) = titled_by {
            node = node.from_record(id);
        }
        Materialized::node(node)
    }
}

/// Photos, and the text read from them, in two zones.
struct Photos;

impl Materialization for Photos {
    fn kind(&self) -> &'static str {
        "photos"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "photos".into(),
            types: vec![tool_type(ATTACHMENT)],
            relations: Vec::new(),
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            Some(Node::new(
                key,
                ATTACHMENT,
                json!({ "title": body(r)["title"], "content": "" }),
            ))
        })
    }
}

struct Ocr;

impl Materialization for Ocr {
    fn kind(&self) -> &'static str {
        "ocr"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "ocr".into(),
            types: vec![tool_type(TEXT)],
            relations: vec![ToolRelation {
                name: "read-from".into(),
                from_type: TEXT.into(),
                to_type: ATTACHMENT.into(),
            }],
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        let Some(r) = records.last() else {
            return Materialized::nothing();
        };
        let b = body(r);
        Materialized {
            nodes: vec![Node::new(key, TEXT, json!({ "content": b["text"] }))],
            relations: vec![Relation::into_zone(
                "read-from",
                key,
                "photos",
                b["photo"].as_str().expect("a text names its photo"),
            )],
        }
    }
}

/// Returns whatever node ids its records name, for the ownership rule.
struct Echo;

impl Materialization for Echo {
    fn kind(&self) -> &'static str {
        "echo"
    }

    fn ontology(&self) -> ToolOntology {
        ToolOntology {
            kind: "echo".into(),
            types: vec![tool_type(NOTE)],
            relations: Vec::new(),
        }
    }

    fn materialize(&self, key: &str, records: &[Record]) -> Materialized {
        newest(records, |r| {
            Some(Node::new(
                body(r)["id"].as_str()?,
                NOTE,
                json!({ "content": key }),
            ))
        })
    }
}

fn chat() -> Vec<Record> {
    vec![
        record("m1", "alice", 1, json!({ "text": "vet on Friday?" })),
        record(
            "m2",
            "bob",
            2,
            json!({ "text": "I'll take her", "follows": "m1" }),
        ),
    ]
}

// ------------------------------------------------- materialization interface

/// Messages about no subject are found by their type, and no subject is made for them.
#[test]
fn messages_are_read_with_no_subject_and_found_by_their_type() {
    let db = graph("alice");
    db.catch_up_zone(POD, "chat", &Chat, &chat()).unwrap();

    let zones = [zone(POD, "chat")];
    let messages = db
        .find_in(&[POD.into()], &zones, None, None)
        .expect("the zone reads");
    assert_eq!(ids(&messages), ["m1", "m2"]);
    assert!(messages
        .iter()
        .all(|m| m.subject.is_none() && m.type_id == MESSAGE));
    assert_eq!(messages[1].member, "bob");
    assert_eq!(messages[1].device.as_deref(), Some("bob-phone"));
    assert_eq!(messages[1].value, json!({ "content": "I'll take her" }));

    let by_subject = db
        .find_in(
            &[POD.into()],
            &zones,
            None,
            Some(&["conversation".to_owned()]),
        )
        .unwrap();
    assert_eq!(by_subject, [], "no message is about a subject");
    assert_eq!(
        db.subjects_in(&[POD.into()], &zones).unwrap(),
        [],
        "no subject is made for messages"
    );
}

/// A relation leads from the message that follows to the one it follows, and back.
#[test]
fn a_relation_is_followed_both_ways() {
    let db = graph("alice");
    db.catch_up_zone(POD, "chat", &Chat, &chat()).unwrap();
    assert_eq!(
        related(&db, &["chat"], "chat", "m2", "follows", Direction::Out),
        ["m1"]
    );
    assert_eq!(
        related(&db, &["chat"], "chat", "m1", "follows", Direction::In),
        ["m2"]
    );
    assert_eq!(
        related(&db, &["chat"], "chat", "m1", "follows", Direction::Out),
        [] as [std::string::String; 0]
    );
}

/// A relation into another pod-tool's zone leads to its target once that zone is read, and to nothing for a view that does not reach it.
#[test]
fn a_relation_leads_into_another_zone_only_for_a_handle_that_reaches_it() {
    let db = graph("alice");
    db.catch_up_zone(
        POD,
        "ocr",
        &Ocr,
        &[record(
            "t1",
            "alice",
            2,
            json!({ "text": "Invoice: 420 SEK", "photo": "p7" }),
        )],
    )
    .unwrap();

    // The zone `photos` is not read yet: the relation is kept, and leads to
    // nothing until it is.
    assert_eq!(
        related(
            &db,
            &["ocr", "photos"],
            "ocr",
            "t1",
            "read-from",
            Direction::Out
        ),
        [] as [std::string::String; 0]
    );
    db.catch_up_zone(
        POD,
        "photos",
        &Photos,
        &[record("p7", "alice", 1, json!({ "title": "vet invoice" }))],
    )
    .unwrap();

    assert_eq!(
        related(
            &db,
            &["ocr", "photos"],
            "ocr",
            "t1",
            "read-from",
            Direction::Out
        ),
        ["p7"]
    );
    assert_eq!(
        related(&db, &["ocr"], "ocr", "t1", "read-from", Direction::Out),
        Vec::<String>::new(),
        "photos is out of this handle's reach"
    );
    assert_eq!(
        related(
            &db,
            &["ocr", "photos"],
            "photos",
            "p7",
            "read-from",
            Direction::In
        ),
        ["t1"]
    );
}

/// A document is made from every operation under its key, and takes its member from the record it names.
#[test]
fn a_document_is_made_from_every_operation_under_its_key() {
    let db = graph("alice");
    let ops = [
        record("d1", "alice", 1, json!({ "title": "Ginger's diet" })),
        record("d1", "bob", 2, json!({ "add": "no fish" })),
    ];
    db.catch_up_zone(POD, "docs", &Docs, &ops).unwrap();

    let docs = all(&db, &["docs"]);
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0].value,
        json!({ "title": "Ginger's diet", "content": "no fish" })
    );
    // Its member and time are the record it names: the one that set its title.
    assert_eq!(docs[0].member, "alice");
    assert_eq!(docs[0].device.as_deref(), Some("alice-phone"));
}

/// An edited note holds its newest text, with the member of its newest record.
#[test]
fn a_node_takes_its_member_from_the_newest_record_by_default() {
    let db = graph("alice");
    let notes = [
        record("n1", "alice", 1, json!({ "text": "vet on Friday" })),
        record("n1", "bob", 2, json!({ "text": "vet on Saturday" })),
    ];
    db.catch_up_zone(POD, "notes", &Notes, &notes).unwrap();
    let read = all(&db, &["notes"]);
    assert_eq!(read.len(), 1);
    assert_eq!(
        (read[0].member.as_str(), &read[0].value),
        ("bob", &json!({ "content": "vet on Saturday" }))
    );
    assert_eq!(
        read[0].subject.as_ref().map(|s| s.id.as_str()),
        Some("ginger")
    );
}

/// A key whose newest record stands for nothing holds no node.
#[test]
fn a_key_whose_records_stand_for_nothing_holds_nothing() {
    let db = graph("alice");
    let notes = [
        record("n1", "alice", 1, json!({ "text": "vet on Friday" })),
        record("n2", "alice", 2, json!({ "text": "wet food" })),
    ];
    db.catch_up_zone(POD, "notes", &Notes, &notes).unwrap();

    // A newer record under n2 that stands for nothing (no text).
    let mut later = notes.to_vec();
    later.push(record("n2", "alice", 3, json!({ "removed": true })));
    db.catch_up_zone(POD, "notes", &Notes, &later).unwrap();
    assert_eq!(ids(&all(&db, &["notes"])), ["n1"]);
}

// ---------------------------------------------------------------- catch-up

/// Records caught up twice apply nothing, and a key no record is under any more is taken out with its relations.
#[test]
fn the_same_records_twice_apply_nothing_and_a_gone_key_is_taken_out() {
    let db = graph("alice");
    let records = chat();
    let first = db.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
    assert_eq!(first.appended, 2);
    let again = db.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
    assert_eq!((again.appended, again.replaced, again.removed), (0, 0, 0));

    let gone = db.catch_up_zone(POD, "chat", &Chat, &records[..1]).unwrap();
    assert_eq!(gone.removed, 1);
    assert_eq!(ids(&all(&db, &["chat"])), ["m1"]);
    assert_eq!(
        related(&db, &["chat"], "chat", "m1", "follows", Direction::In),
        Vec::<String>::new(),
        "m2's relation went with it"
    );
}

/// A key with a newer record holds what that record stands for, its old relation gone.
#[test]
fn a_key_with_a_newer_record_is_applied_again_whole() {
    let db = graph("alice");
    let mut records = chat();
    db.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
    assert_eq!(
        related(&db, &["chat"], "chat", "m2", "follows", Direction::Out),
        ["m1"]
    );

    records.push(record(
        "m2",
        "bob",
        3,
        json!({ "text": "I'll take her on Saturday" }),
    ));
    let applied = db.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
    assert_eq!((applied.replaced, applied.appended), (1, 1));

    let m2 = all(&db, &["chat"])
        .into_iter()
        .find(|n| n.node_id == "m2")
        .unwrap();
    assert_eq!(m2.value, json!({ "content": "I'll take her on Saturday" }));
    assert_eq!(
        related(&db, &["chat"], "chat", "m2", "follows", Direction::Out),
        Vec::<String>::new(),
        "the old relation is gone"
    );
}

/// A node id two keys return is kept from the key first in byte order.
#[test]
fn a_node_id_two_keys_return_is_kept_from_the_first_key() {
    let db = graph("alice");
    let records = [
        record("b", "alice", 1, json!({ "id": "x" })),
        record("a", "bob", 2, json!({ "id": "x" })),
    ];
    let applied = db.catch_up_zone(POD, "echo", &Echo, &records).unwrap();
    assert_eq!(applied.skipped, 1);
    let read = all(&db, &["echo"]);
    assert_eq!(read.len(), 1);
    assert_eq!(
        read[0].value,
        json!({ "content": "a" }),
        "key `a` sorts first and keeps `x`"
    );
}

/// A node of an undeclared type, a value that does not fit and an undeclared relation are left out, and the rest loads.
#[test]
fn what_its_tool_ontology_does_not_declare_is_left_out() {
    struct Loose;
    impl Materialization for Loose {
        fn kind(&self) -> &'static str {
            "loose"
        }
        fn ontology(&self) -> ToolOntology {
            ToolOntology {
                kind: "loose".into(),
                types: vec![tool_type(NOTE)],
                relations: Vec::new(),
            }
        }
        fn materialize(&self, key: &str, _: &[Record]) -> Materialized {
            Materialized {
                nodes: vec![
                    Node::new(format!("{key}-note"), NOTE, json!({ "content": "kept" })),
                    Node::new(
                        format!("{key}-msg"),
                        MESSAGE,
                        json!({ "content": "not declared" }),
                    ),
                    Node::new(format!("{key}-bad"), NOTE, json!({ "species": "cat" })),
                ],
                relations: vec![Relation::new(
                    "undeclared",
                    format!("{key}-note"),
                    format!("{key}-note"),
                )],
            }
        }
    }
    let db = graph("alice");
    let applied = db
        .catch_up_zone(POD, "loose", &Loose, &[record("k", "alice", 1, json!({}))])
        .unwrap();
    assert_eq!(
        applied.skipped, 3,
        "an undeclared type, a value that does not fit, an undeclared relation"
    );
    assert_eq!(ids(&all(&db, &["loose"])), ["k-note"]);
}

/// A node for two members is in their graphs and not a third member's, where a relation to it leads to nothing.
#[test]
fn a_node_for_some_members_is_built_only_into_their_graphs() {
    let records = vec![
        record("m1", "alice", 1, json!({ "text": "vet on Friday?" })),
        record(
            "dm1",
            "alice",
            2,
            json!({ "text": "Dan's gift: the blue scarf", "to": ["alice", "bob"] }),
        ),
        record("m3", "bob", 3, json!({ "text": "sure", "follows": "dm1" })),
    ];
    for (who, sees_dm) in [("alice", true), ("bob", true), ("dan", false)] {
        let db = graph(who);
        let applied = db.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
        let read = all(&db, &["chat"]);
        assert_eq!(read.iter().any(|n| n.node_id == "dm1"), sees_dm, "{who}");
        assert_eq!(applied.withheld, usize::from(!sees_dm), "{who}");
        let followed = related(&db, &["chat"], "chat", "m3", "follows", Direction::Out);
        assert_eq!(
            followed.len(),
            usize::from(sees_dm),
            "{who}: the relation to it leads to nothing elsewhere"
        );
        if let Some(dm) = read.iter().find(|n| n.node_id == "dm1") {
            assert_eq!(dm.readers, ["alice", "bob"]);
        }
    }
}

/// The same keys bulk-loaded and applied one at a time make the same graph, relations included.
#[test]
fn bulk_loaded_and_applied_keys_are_the_same_graph() {
    let mut records = Vec::new();
    for i in 0..40 {
        let follows = if i > 0 {
            json!(format!("m{:02}", i - 1))
        } else {
            Value::Null
        };
        records.push(record(
            &format!("m{i:02}"),
            if i % 2 == 0 { "alice" } else { "bob" },
            i,
            json!({ "text": format!("message {i}"), "follows": follows }),
        ));
    }
    let bulk = graph("alice").with_bulk_threshold(10);
    let one_by_one = graph("alice").with_bulk_threshold(1_000);
    let a = bulk.catch_up_zone(POD, "chat", &Chat, &records).unwrap();
    let b = one_by_one
        .catch_up_zone(POD, "chat", &Chat, &records)
        .unwrap();
    assert_eq!((a.bulk, a.appended), (40, 0));
    assert_eq!((b.bulk, b.appended), (0, 40));
    assert_eq!(all(&bulk, &["chat"]), all(&one_by_one, &["chat"]));
    for i in 1..40 {
        let id = format!("m{i:02}");
        assert_eq!(
            related(&bulk, &["chat"], "chat", &id, "follows", Direction::Out),
            related(
                &one_by_one,
                &["chat"],
                "chat",
                &id,
                "follows",
                Direction::Out
            )
        );
    }
    assert_eq!(
        related(&bulk, &["chat"], "chat", "m39", "follows", Direction::Out),
        ["m38"]
    );
}

/// Five small zones new to the graph load in one bulk load.
#[test]
fn many_small_new_zones_load_together() {
    let db = graph("alice").with_bulk_threshold(10);
    let pods = ["a", "b", "c", "d", "e"];
    let records: Vec<Vec<Record>> = pods
        .iter()
        .map(|_| {
            (0..4)
                .map(|i| {
                    record(
                        &format!("n{i}"),
                        "alice",
                        i,
                        json!({ "text": format!("note {i}") }),
                    )
                })
                .collect()
        })
        .collect();
    let zones: Vec<_> = pods
        .iter()
        .zip(&records)
        .map(|(pod, r)| (*pod, "notes", &Notes as &dyn Materialization, r.as_slice()))
        .collect();
    let applied = db.catch_up_many(&zones).unwrap();
    assert_eq!((applied.bulk, applied.appended), (20, 0));
    for pod in pods {
        assert_eq!(
            db.find_in(&[pod.into()], &[zone(pod, "notes")], None, None)
                .unwrap()
                .len(),
            4
        );
    }
}
