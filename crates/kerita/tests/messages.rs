//! The corpus' messages through the crate: every message keripy wrote for an event of the subset
//! reads to that event and its attachments, and writes back to its own bytes.

mod support;

use kerita::event::Limits;
use kerita::message::Message;
use kerita::primitive::{IndexedSignature, Refusal};
use kerita::state::{Context, KeyState};
use serde_json::Value;

const LIMITS: Limits = Limits {
    body: 16_384,
    list: 384,
    attachments: 4_096,
};

fn texts(signatures: &[IndexedSignature]) -> Value {
    signatures
        .iter()
        .map(|s| Value::from(s.to_text()))
        .collect()
}

/// Every message keripy wrote while its validators were given the KELs of the subset reads to
/// keripy's event, controller signatures, witness signatures and delegation source, and writes
/// back to its own bytes.
#[test]
fn messages_read_as_keripy_writes_them() {
    let mut read = 0;
    for file in support::manifest("keripy").unwrap().into_keys() {
        for v in support::vectors("keripy", &file).unwrap() {
            if v["kind"] != "kel" || v["expect"] != "same" {
                continue;
            }
            let id = v["id"].as_str().unwrap().to_owned();
            for (n, step) in v["input"]["steps"].as_array().unwrap().iter().enumerate() {
                // keripy writes no message whose receipt is a transferable AID's.
                let Some(raw) = step["message"].as_str() else {
                    continue;
                };
                let (message, used) = Message::read(raw.as_bytes(), &LIMITS)
                    .map_err(|e| format!("{id} step {n}: {e}"))
                    .unwrap();
                assert_eq!(used, raw.len(), "{id} step {n}: bytes read");
                assert_eq!(
                    message.event().text(),
                    step["event"],
                    "{id} step {n}: event"
                );
                assert_eq!(
                    texts(message.signatures()),
                    step["sigs"],
                    "{id} step {n}: signatures"
                );
                assert_eq!(
                    texts(message.witness_signatures()),
                    step["wigs"],
                    "{id} step {n}: witnesses"
                );
                let source = message.source().map_or(
                    Value::Null,
                    |s| serde_json::json!({ "s": format!("{:x}", s.sn), "d": s.said.to_text() }),
                );
                assert_eq!(
                    source, step["delegation"],
                    "{id} step {n}: delegation source"
                );
                assert_eq!(
                    message.to_text().unwrap(),
                    raw,
                    "{id} step {n}: written back"
                );
                read += 1;
            }
        }
    }
    assert_ne!(read, 0);
}

/// keripy's one-key inception, with an attachment group holding one empty group of controller
/// signatures.
const ATTACHED: &str = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA-CAB-KAA";

/// Attachments declared longer than the caller's limit are refused over the limit, though their
/// content is all there.
#[test]
fn oversized_attachments_are_refused_before_their_content_is_read() {
    let limits = Limits {
        attachments: 0,
        ..LIMITS
    };
    assert!(matches!(
        Message::read(ATTACHED.as_bytes(), &limits),
        Err(Refusal::OverLimit(_))
    ));
    assert!(Message::read(ATTACHED.as_bytes(), &LIMITS).is_ok());
}

/// Bytes past a message are not read: bytes that are no text there leave the message read, with
/// or without attachments, for a stream to go on from.
#[test]
fn bytes_past_a_message_are_not_read() {
    for (message, length) in [(&ATTACHED[..188], 188), (ATTACHED, 196)] {
        let mut raw = message.as_bytes().to_vec();
        raw.extend_from_slice(&[0x80, 0xff]);
        let (_, used) = Message::read(&raw, &LIMITS).unwrap();
        assert_eq!(used, length);
    }
}

/// Every stream of the corpus that carries a genus-version code, read message by message and
/// each accepted in turn, reaches the key state keripy's parser reached: past a code naming CESR
/// 2.00, before the first message or between two, and to nothing past one naming another
/// version, or past 2 in a row.
#[test]
fn streams_read_as_keripy_reads_them() {
    let vectors = support::vectors("keripy", "stream.jsonl").unwrap();
    assert_ne!(vectors.len(), 0, "no stream in the corpus");
    for vector in vectors {
        let id = vector["id"].as_str().unwrap();
        let mut rest = vector["input"]["stream"].as_str().unwrap().as_bytes();
        let mut state: Option<KeyState> = None;
        let mut events = Vec::new();
        while let Ok((message, used)) = Message::read(rest, &LIMITS) {
            let context = Context {
                accepted: &events,
                ..Context::default()
            };
            let Ok(next) = KeyState::accept(state.as_ref(), &message, context) else {
                break;
            };
            events.push(message.event().clone());
            state = Some(next);
            rest = &rest[used..];
        }
        let theirs = vector["keripy"]["sn"].as_u64().map(u128::from);
        assert_eq!(state.map(|s| s.sn()), theirs, "{id}");
    }
}

/// A genus-version code naming CESR 1.00 before a message is refused as unsupported, where one
/// naming CESR 2.00 is read past.
#[test]
fn a_genus_version_of_another_version_is_unsupported() {
    let raw = format!("-_AAABAA{ATTACHED}");
    assert!(matches!(
        Message::read(raw.as_bytes(), &LIMITS),
        Err(Refusal::Unsupported(_))
    ));
    let raw = format!("-_AAACAA{ATTACHED}");
    let (_, used) = Message::read(raw.as_bytes(), &LIMITS).unwrap();
    assert_eq!(used, raw.len());
}
