//! A native event rendered field by field, for a person reading a KEL and for the message of a
//! failed comparison with keripy.

use alloc::string::String;
use alloc::vec::Vec;

use crate::event::{Event, Limits};

const LIMITS: Limits = Limits {
    body: usize::MAX / 8,
    list: usize::MAX / 8,
    attachments: 0,
};

/// One line per field — its label, then its text — or why the bytes are no event.
pub fn annotate(bytes: &[u8]) -> String {
    match lines(bytes) {
        Ok(lines) => lines.join("\n"),
        Err(refusal) => alloc::format!("not an event: {refusal}"),
    }
}

/// The label of the first field where `ours` and `theirs` differ — past the SAID, which differs
/// whenever another field does — with both rendered, or `None` when their bytes agree.
pub fn first_difference(ours: &[u8], theirs: &[u8]) -> Option<String> {
    if ours == theirs {
        return None;
    }
    let (a, b) = (
        lines(ours).unwrap_or_default(),
        lines(theirs).unwrap_or_default(),
    );
    let differing: Vec<&str> = a
        .iter()
        .zip(&b)
        .filter(|(x, y)| x != y)
        .map(|(x, _)| x.split(' ').next().unwrap_or("?"))
        .collect();
    // Any change moves the SAID, and a self-addressing prefix with it: name what moved them.
    let label = differing
        .iter()
        .find(|l| !matches!(**l, "d" | "i"))
        .or_else(|| differing.first())
        .copied()
        .unwrap_or("the length");
    Some(alloc::format!(
        "first difference: {label}\nours:\n{}\ntheirs:\n{}",
        annotate(ours),
        annotate(theirs)
    ))
}

fn lines(bytes: &[u8]) -> Result<Vec<String>, crate::primitive::Refusal> {
    let (event, _) = Event::read(bytes, &LIMITS)?;
    Ok(event
        .fields()?
        .into_iter()
        .map(|(label, text)| alloc::format!("{label:<2} {text}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICP: &str = "-FAu0OKERICAACAAXicpENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4DG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAAMAAB-JALDG9XhvcVryHjoIGcj5nK4sAE3oslQHWi4fBJre3NGwTQMAAA-JAAMAAA-JAA-JAA-JAA";

    /// Rendered, keripy's one-key inception names its fields in order, its SAID among them.
    #[test]
    fn an_event_renders_one_field_a_line() {
        let rendered = annotate(ICP.as_bytes());
        assert!(
            rendered.starts_with("v  0OKERICAACAA\nt  Xicp\nd  ENcf5ii1"),
            "{rendered}"
        );
    }

    /// A wrong current threshold is named as the first field that differs.
    #[test]
    fn a_difference_is_named_by_field() {
        use crate::event::{Draft, Ilk};
        use crate::threshold::Threshold;
        let (event, _) = Event::read(ICP.as_bytes(), &LIMITS).unwrap();
        let wrong = Event::build(
            Ilk::Inception,
            Draft {
                prefix: Some(event.prefix().clone()),
                key_threshold: Some(Threshold::Count(2)),
                keys: event.keys().to_vec(),
                next_threshold: Some(Threshold::Count(0)),
                ..Draft::default()
            },
        )
        .unwrap();
        let report = first_difference(wrong.text().as_bytes(), ICP.as_bytes()).unwrap();
        assert!(report.starts_with("first difference: kt"), "{report}");
    }
}
