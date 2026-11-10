//! The entry points the fuzz targets drive (`fuzz/`), shared with the replay of what they found
//! (`tests/fuzz_replay.rs`). Each answers whether the input was read, and never panics.

use crate::event::{Event, Limits};
use crate::message::Message;
use crate::primitive::{IndexedSignature, Primitive};

const LIMITS: Limits = Limits {
    body: 4_096,
    list: 384,
    attachments: 4_096,
};

/// keripy's one-key inception, which the attachments target puts before its input.
const INCEPTION: &str = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA";

pub fn primitive(data: &[u8]) -> bool {
    core::str::from_utf8(data)
        .is_ok_and(|text| Primitive::read(text).is_ok() | IndexedSignature::read(text).is_ok())
}

pub fn attachments(data: &[u8]) -> bool {
    let mut message = alloc::vec::Vec::from(INCEPTION.as_bytes());
    message.extend_from_slice(data);
    Message::read(&message, &LIMITS).is_ok()
}

pub fn event(data: &[u8]) -> bool {
    Event::read(data, &LIMITS).is_ok()
}

/// Whether an event read from `data` writes back to the bytes it was read from; an input the
/// crate refuses is no counterexample.
pub fn round_trips(data: &[u8]) -> bool {
    Event::read(data, &LIMITS).map_or(true, |(event, used)| {
        event
            .to_text()
            .is_ok_and(|text| data.get(..used) == Some(text.as_bytes()))
    })
}

/// A run of messages, as a peer sends a KEL: read until one is refused or the input ends.
pub fn stream(data: &[u8]) -> bool {
    let mut rest = data;
    while !rest.is_empty() {
        match Message::read(rest, &LIMITS) {
            Ok((_, used)) if used > 0 => rest = rest.get(used..).unwrap_or_default(),
            _ => return false,
        }
    }
    true
}

/// The target of each name the fuzz crate and the replay use.
pub fn target(name: &str) -> Option<fn(&[u8]) -> bool> {
    match name {
        "primitive" => Some(primitive),
        "attachments" => Some(attachments),
        "event" => Some(event),
        "stream" => Some(stream),
        _ => None,
    }
}
