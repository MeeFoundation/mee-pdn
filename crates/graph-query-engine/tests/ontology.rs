//! The ontology registry: the types the graph is typed by, and the check of
//! a value against its type (the mia-docs change `graph-query-engine`,
//! capability `ontology-registry`).

use graph_query_engine::db::GraphDb;
use graph_query_engine::{check, Ontology, TypeId};
use serde_json::json;

const APP_ONTOLOGY: &str = include_str!("../ontology/app-ontology.json");

fn db() -> GraphDb {
    GraphDb::in_memory(&Ontology::platform()).expect("the graph opens")
}

fn refusal(db: &GraphDb, type_id: &str, value: serde_json::Value) -> String {
    check(
        &db.shape(&TypeId::new(type_id))
            .expect("the type has a shape"),
        &value,
    )
    .expect_err("the value is refused")
    .to_string()
}

/// The application ontology loads on top of mia-ontologies, whose types stay beside it.
#[test]
fn the_application_ontology_loads_on_top_of_mia_ontologies() {
    let platform = Ontology::platform();
    let types: Vec<&str> = platform
        .tables()
        .types
        .iter()
        .map(|t| t.type_id.as_str())
        .collect();
    for app in [
        "app:UnstructuredText:v1",
        "app:Message:v1",
        "app:Note:v1",
        "app:CanvasElement:v1",
        "app:Attachment:v1",
        "app:CalendarEvent:v1",
        "app:Recurrence:v1",
    ] {
        assert!(types.contains(&app), "{app}");
    }
    assert!(
        types.contains(&"mee:PetMedicationRecord:v1"),
        "mia-ontologies' types beside them"
    );
    assert!(
        Ontology::mee()
            .tables()
            .types
            .iter()
            .all(|t| !t.type_id.starts_with("app:")),
        "mia-ontologies alone holds no app: type"
    );
}

/// A registry that defines a type twice is refused whole.
#[test]
fn a_registry_that_defines_a_type_twice_is_refused() {
    let refused = Ontology::layered(&[APP_ONTOLOGY, APP_ONTOLOGY])
        .err()
        .expect("refused whole");
    assert!(refused.contains("defined twice"), "{refused}");
}

/// A message's generalizations are the message and text, nearest first.
#[test]
fn a_message_is_read_as_text() {
    assert_eq!(
        db().generalizations("app:Message:v1").unwrap(),
        ["app:Message:v1", "app:UnstructuredText:v1"]
    );
}

/// A value with a field its type does not have is refused, naming the field.
#[test]
fn a_value_with_a_field_its_type_does_not_have_is_refused() {
    let refused = refusal(
        &db(),
        "mee:PetMedicationRecord:v1",
        json!({ "species": "cat" }),
    );
    assert!(refused.contains("species"), "{refused}");
}

/// A value missing a required field, with one value for a list or a wrong primitive is refused, a type with no fields takes none, and a fitting value comes back unchanged.
#[test]
fn a_value_is_refused_for_a_missing_field_a_wrong_cardinality_or_a_wrong_primitive_and_a_type_with_no_fields_takes_none(
) {
    let db = db();
    let missing = refusal(&db, "app:Note:v1", json!({ "title": "vet" }));
    assert!(
        missing.contains("value.content") && missing.contains("required"),
        "{missing}"
    );
    let one_for_many = refusal(
        &db,
        "mee:PetMedicationRecord:v1",
        json!({ "medication": { "brand_name": "Clavamox" } }),
    );
    assert!(
        one_for_many.contains("value.medication") && one_for_many.contains("list"),
        "{one_for_many}"
    );
    let primitive = refusal(&db, "app:Note:v1", json!({ "content": 42 }));
    assert!(
        primitive.contains("value.content") && primitive.contains("string"),
        "{primitive}"
    );
    let no_fields = refusal(&db, "mee:Subject:v1", json!({}));
    assert!(no_fields.contains("no fields"), "{no_fields}");
    // Nothing is coerced: a value that fits comes back as it went in.
    let fits = json!({ "content": "vet on Friday" });
    assert_eq!(
        check(&db.shape(&TypeId::new("app:Note:v1")).unwrap(), &fits).unwrap(),
        fits
    );
}

/// Shapes, entity types and subject types are read from the registry in the graph, and an unknown name is refused.
#[test]
fn shapes_entity_types_and_subject_types_come_from_the_registry_in_the_graph() {
    let db = db();
    let shape = db
        .shape(&TypeId::new("mee:PetMedicationRecord:v1"))
        .unwrap();
    assert_eq!(
        shape
            .field("medication")
            .unwrap()
            .child_shape
            .as_deref()
            .unwrap()
            .type_info
            .name,
        "Medication"
    );
    let entity_types: Vec<String> = db
        .entity_types()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(entity_types.contains(&"PetMedicationRecord".to_owned()));
    let subject_types: Vec<String> = db
        .subject_types()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        subject_types.contains(&"Person".to_owned())
            && !subject_types.contains(&"PetMedicationRecord".to_owned())
    );
    assert!(db.get_type_by_name("NoSuchType").is_err());
}

/// The application ontology with `change` applied, as a registry of its own.
fn broken(change: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut registry: serde_json::Value =
        serde_json::from_str(APP_ONTOLOGY).expect("the application ontology is JSON");
    change(&mut registry);
    Ontology::layered(&[&registry.to_string()])
        .err()
        .expect("refused whole")
}

/// A registry whose relation names an unknown type or field, or whose field has no value type, is refused whole.
#[test]
fn a_registry_whose_relation_names_what_it_does_not_hold_or_whose_field_has_no_value_type_is_refused(
) {
    assert!(
        Ontology::layered(&[APP_ONTOLOGY]).is_ok(),
        "the application ontology stands on its own"
    );
    let unknown_type = broken(|r| {
        let field = r["fields"][0]["field_id"].clone();
        r["type_has_field"]
            .as_array_mut()
            .unwrap()
            .push(json!(["app:NoSuchType:v1", field]));
    });
    assert!(unknown_type.contains("unknown type"), "{unknown_type}");
    let unknown_field = broken(|r| {
        r["type_has_field"]
            .as_array_mut()
            .unwrap()
            .push(json!(["app:Note:v1", "no-such-field"]))
    });
    assert!(unknown_field.contains("unknown field"), "{unknown_field}");
    let untyped = broken(|r| {
        r["field_value_type"].as_array_mut().unwrap().remove(0);
    });
    assert!(untyped.contains("has no FIELD_VALUE_TYPE"), "{untyped}");
}
