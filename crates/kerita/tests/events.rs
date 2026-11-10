//! The corpus' events through the crate: an event of the subset reads to keripy's fields and
//! writes back to its own bytes; one outside the subset is refused as unsupported; one keripy
//! refuses, the crate refuses.

mod support;

use kerita::annotate::first_difference;
use kerita::event::{Event, Limits, Seal};
use kerita::primitive::{Primitive, Refusal};
use kerita::threshold::Threshold;
use serde_json::{Value, json};

/// 32 P-256 keys of 12 quadlets each, as the encoding spec's example limits a list.
const LIMITS: Limits = Limits {
    body: 16_384,
    list: 384,
    attachments: 4_096,
};

fn texts(items: &[Primitive]) -> Value {
    items.iter().map(|p| Value::from(p.to_text())).collect()
}

fn sith(threshold: Option<&Threshold>) -> Value {
    match threshold {
        None => Value::Null,
        Some(Threshold::Count(n)) => Value::from(format!("{n:x}")),
        Some(Threshold::Weighted(clauses)) => {
            let clauses: Vec<Value> = clauses
                .as_slice()
                .iter()
                .map(|c| c.iter().map(|w| Value::from(w.to_string())).collect())
                .collect();
            match clauses.as_slice() {
                [one] => one.clone(),
                _ => Value::from(clauses),
            }
        }
    }
}

fn seals(seals: &[Seal]) -> Value {
    seals
        .iter()
        .map(|s| match s {
            Seal::Digest(d) => json!({ "d": d.to_text() }),
            Seal::Event { prefix, sn, said } => {
                json!({ "i": prefix.to_text(), "s": format!("{sn:x}"), "d": said.to_text() })
            }
        })
        .collect()
}

/// The event's fields in keripy's form, the labels keripy's `sad` holds for its type.
fn fields(e: &Event) -> Value {
    let mut sad = json!({
        "t": e.ilk().tag(), "d": e.said().to_text(), "i": e.prefix().to_text(),
        "s": format!("{:x}", e.sn()),
    });
    let map = sad.as_object_mut().unwrap_or_else(|| unreachable!());
    if let Some(p) = e.prior() {
        map.insert("p".into(), p.to_text().into());
    }
    if e.ilk().is_establishment() {
        map.insert("kt".into(), sith(e.key_threshold()));
        map.insert("k".into(), texts(e.keys()));
        map.insert("nt".into(), sith(e.next_threshold()));
        map.insert("n".into(), texts(e.next_digests()));
        map.insert(
            "bt".into(),
            format!("{:x}", e.witness_threshold().unwrap_or(0)).into(),
        );
        if e.ilk().is_inception() {
            map.insert("b".into(), texts(e.witnesses()));
        } else {
            map.insert("br".into(), texts(e.cuts()));
            map.insert("ba".into(), texts(e.adds()));
        }
        map.insert(
            "c".into(),
            e.traits().iter().map(|t| Value::from(t.as_str())).collect(),
        );
    }
    if e.ilk().tag() != "rct" {
        map.insert("a".into(), seals(e.seals()));
    }
    if let Some(di) = e.delegator() {
        map.insert("di".into(), di.to_text().into());
    }
    sad
}

/// keripy's `sad` without its version field, which the native body carries as a primitive, with
/// a threshold keripy holds as an integer written in hexadecimal, and an empty list as a list.
fn keripy_fields(sad: &Value) -> Value {
    let mut sad = sad.clone();
    if let Some(map) = sad.as_object_mut() {
        map.remove("v");
        for field in ["kt", "nt", "bt"] {
            if let Some(n) = map.get(field).and_then(Value::as_u64) {
                map.insert(field.into(), format!("{n:x}").into());
            }
        }
        // keripy keeps the empty sequence number its test gave, and writes it as 0.
        if map.get("s") == Some(&Value::from("")) {
            map.insert("s".into(), "0".into());
        }
        // keripy holds an empty list it read from a native body as an empty string.
        for field in ["k", "n", "b", "br", "ba", "c", "a"] {
            if map.get(field) == Some(&Value::from("")) {
                map.insert(field.into(), Value::Array(Vec::new()));
            }
        }
    }
    sad
}

/// Every event keripy built reads to keripy's SAID and fields and writes back to its own bytes
/// when it is in the subset; one outside the subset is refused as unsupported; one keripy
/// refuses, the crate refuses.
#[test]
fn events_read_as_keripy_reads_them() {
    let mut counts = [0usize; 3];
    for directory in support::DIRECTORIES {
        for file in support::manifest(directory).unwrap().into_keys() {
            for v in support::vectors(directory, &file).unwrap() {
                if v["kind"] != "event" {
                    continue;
                }
                let id = format!("{directory}/{}", v["id"].as_str().unwrap());
                let raw = v["input"]["raw"].as_str().unwrap();
                let answer = Event::read(raw.as_bytes(), &LIMITS);
                match (v["expect"].as_str().unwrap(), answer) {
                    ("same", Ok((event, used))) => {
                        assert_eq!(used, raw.len(), "{id}: bytes read");
                        assert_eq!(event.said().to_text(), v["keripy"]["said"], "{id}: SAID");
                        assert_eq!(
                            fields(&event),
                            keripy_fields(&v["keripy"]["sad"]),
                            "{id}: fields"
                        );
                        let written = event.to_text().unwrap();
                        let report = first_difference(written.as_bytes(), raw.as_bytes());
                        assert!(
                            report.is_none(),
                            "{id}: written back\n{}",
                            report.unwrap_or_default()
                        );
                        counts[0] += 1;
                    }
                    ("unsupported", Err(Refusal::Unsupported(_))) => counts[1] += 1,
                    ("refused", Err(_)) => counts[2] += 1,
                    (expect, answer) => {
                        let answered = format!("{answer:?}");
                        assert_eq!(
                            expect, answered,
                            "{id}: the crate's answer against the expected one"
                        );
                    }
                }
            }
        }
    }
    assert!(
        counts.iter().all(|&n| n > 0),
        "same, unsupported, refused: {counts:?}"
    );
}

/// keripy's one-key inception from `test_keri_native_dumps_loads` reads and writes back as its
/// own 188 bytes.
#[test]
fn the_one_key_inception_reads_and_writes_back() {
    let raw = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA";
    let (event, used) = Event::read(raw.as_bytes(), &LIMITS).unwrap();
    assert_eq!(used, 188);
    assert_eq!(
        event.said().to_text(),
        "ENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4"
    );
    assert_eq!(event.to_text().unwrap(), raw);
}

/// A KERI 1.0 event and a KERI 2.00 JSON body are refused as unsupported, not as malformed.
#[test]
fn other_versions_and_bodies_are_unsupported() {
    for raw in [
        r#"{"v":"KERI10JSON0000cf_","t":"icp"}"#,
        r#"{"v":"KERICAACAAJSONAADR.","t":"icp"}"#,
        "-FAtYKERICAAXicpEEx4oxGYbNrd6nZsdGu2KdN4MSDGD5IWS7hXjST7r8ewDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-IALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-IAAMAAA-IAA-IAA-IAA",
    ] {
        assert!(
            matches!(
                Event::read(raw.as_bytes(), &LIMITS),
                Err(Refusal::Unsupported(_))
            ),
            "{raw}"
        );
    }
}

/// An event cut in the middle of its key list is refused as malformed.
#[test]
fn a_broken_event_is_malformed() {
    let raw = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JAL";
    assert!(matches!(
        Event::read(raw.as_bytes(), &LIMITS),
        Err(Refusal::Malformed(_))
    ));
}

/// A key list declared 4,095 quadlets long is refused over the limit, and so before its content
/// is read: the content is not there to read.
#[test]
fn an_oversized_list_is_refused_before_its_content_is_read() {
    let raw = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-J__DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA";
    assert!(matches!(
        Event::read(raw.as_bytes(), &LIMITS),
        Err(Refusal::OverLimit(_))
    ));
}

/// A body declared longer than the caller's limit is refused over the limit, though its content
/// is all there.
#[test]
fn an_oversized_body_is_refused_before_its_content_is_read() {
    let raw = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA";
    let limits = Limits { body: 45, ..LIMITS };
    assert!(matches!(
        Event::read(raw.as_bytes(), &limits),
        Err(Refusal::OverLimit(_))
    ));
    assert!(Event::read(raw.as_bytes(), &LIMITS).is_ok());
}

/// Bytes past an event are not read: bytes that are no text there leave the event read.
#[test]
fn bytes_past_an_event_are_not_read() {
    let mut raw = b"-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA".to_vec();
    raw.extend_from_slice(&[0x80, 0xff]);
    let (_, used) = Event::read(&raw, &LIMITS).unwrap();
    assert_eq!(used, 188);
}
