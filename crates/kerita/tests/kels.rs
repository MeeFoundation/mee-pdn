//! The corpus' KELs through the crate's key state: every step keripy's validators were given gets
//! keripy's answer from the crate, and every accepted step leaves keripy's key state.

mod support;

use std::collections::BTreeMap;

use kerita::event::{Event, Limits};
use kerita::message::{Message, Source};
use kerita::primitive::{IndexedSignature, Primitive, Refusal};
use kerita::state::{Context, KeyState, Validator, Verdict, fold};
use kerita::threshold::Threshold;
use serde_json::{Value, json};

const LIMITS: Limits = Limits {
    body: 16_384,
    list: 384,
    attachments: 4_096,
};

/// One validator's view: each AID's key state and accepted events, and the messages it accepted,
/// each with who it checked the message as.
#[derive(Default)]
struct View {
    kels: BTreeMap<String, (KeyState, Vec<Event>)>,
    accepted: BTreeMap<String, Vec<(Message, Validator)>>,
}

/// Who a step of the corpus was checked by, as the oracle recorded it.
fn checked_by(step: &Value) -> Result<Validator, String> {
    match step["validator"].as_str() {
        Some("remote") => Ok(Validator::Remote),
        Some("controller") => Ok(Validator::Controller),
        Some("witness") => Ok(Validator::Witness),
        other => Err(format!("a step checked by {other:?}")),
    }
}

impl View {
    fn give(&mut self, message: &Message, validator: Validator) -> Result<(), Verdict> {
        let event = message.event();
        let prefix = event.prefix().to_text().to_owned();
        let known = self.kels.get(&prefix);
        let delegator = event
            .delegator()
            .or_else(|| known.and_then(|(s, _)| s.delegator()))
            .map(|d| d.to_text().to_owned());
        let empty = Vec::new();
        let context = Context {
            accepted: known.map_or(&empty, |(_, e)| e),
            delegator: delegator
                .as_ref()
                .and_then(|d| self.kels.get(d))
                .map_or(&empty, |(_, e)| e),
            validator,
        };
        let next = KeyState::accept(known.map(|(s, _)| s), message, context)?;
        self.accepted
            .entry(prefix.clone())
            .or_default()
            .push((message.clone(), validator));
        let entry = self
            .kels
            .entry(prefix)
            .or_insert_with(|| (next.clone(), Vec::new()));
        entry.0 = next;
        entry.1.push(event.clone());
        Ok(())
    }
}

/// keripy's answer for a verdict of the crate's.
fn answer(verdict: &Result<(), Verdict>) -> &'static str {
    match verdict {
        Ok(()) => "accepted",
        Err(Verdict::Duplicate) => "unchanged",
        Err(Verdict::LikelyDuplicitous { .. }) => "LikelyDuplicitousError",
        Err(Verdict::AwaitingPrior { .. }) => "OutOfOrderError",
        Err(Verdict::AwaitingSignatures) => "MissingSignatureError",
        Err(Verdict::AwaitingReceipts) => "MissingWitnessSignatureError",
        Err(Verdict::AwaitingDelegator { .. }) => "MissingDelegationError",
        Err(Verdict::Refused(_)) => "refused",
    }
}

/// keripy's answer, with every refusal of its read as one.
fn keripy_answer(outcome: &str) -> &str {
    match outcome {
        "accepted"
        | "unchanged"
        | "LikelyDuplicitousError"
        | "OutOfOrderError"
        | "MissingSignatureError"
        | "MissingWitnessSignatureError"
        | "MissingDelegationError" => outcome,
        _ => "refused",
    }
}

fn sith(threshold: &Threshold) -> Value {
    match threshold {
        Threshold::Count(n) => Value::from(format!("{n:x}")),
        Threshold::Weighted(clauses) => {
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

fn texts(items: &[Primitive]) -> Value {
    items.iter().map(|p| Value::from(p.to_text())).collect()
}

/// The fields of keripy's key state the crate's key state holds.
fn state(s: &KeyState) -> Value {
    json!({
        "i": s.prefix().to_text(), "s": format!("{:x}", s.sn()), "d": s.said().to_text(),
        "et": s.ilk().tag(), "kt": sith(s.key_threshold()), "k": texts(s.keys()),
        "nt": sith(s.next_threshold()), "n": texts(s.next_digests()),
        "bt": format!("{:x}", s.witness_threshold()), "b": texts(s.witnesses()),
        "c": if s.establishment_only() { json!(["EO"]) } else { json!([]) },
        "di": s.delegator().map_or("", Primitive::to_text),
    })
}

fn keripy_state(s: &Value) -> Value {
    let pick = |f: &str| s.get(f).cloned().unwrap_or(Value::Null);
    let mut picked = json!({});
    for f in [
        "i", "s", "d", "et", "kt", "k", "nt", "n", "bt", "b", "c", "di",
    ] {
        if let Some(map) = picked.as_object_mut() {
            map.insert(f.to_owned(), pick(f));
        }
    }
    picked
}

/// Every step of every KEL of the subset that keripy's validators were given — keripy's own and
/// the ones the crate wrote — gets keripy's answer from the crate, and every step keripy accepted
/// leaves keripy's key state, and an event keripy refuses as it reads it the crate refuses as it
/// reads it too; a KEL outside the subset meets a refusal as unsupported. A KEL
/// resting on keripy's rule for a validator's own events from another source is outside what the
/// crate decides, and is counted apart; one where keripy supersedes an accepted event matches
/// keripy up to that event, which the crate answers as likely duplicitous.
#[test]
#[allow(clippy::too_many_lines)] // one replay: every directory, every kind of vector beside the others
fn kels_fold_as_keripy_folds_them() {
    let mut replayed = [0usize; 3];
    let mut refolded = 0;
    let mut refused_as_read = 0;
    for (directory, file) in ["keripy", "crate"].into_iter().flat_map(|d| {
        support::manifest(d)
            .unwrap()
            .into_keys()
            .map(move |f| (d, f))
    }) {
        for v in support::vectors(directory, &file).unwrap() {
            if v["kind"] != "kel" {
                continue;
            }
            let id = format!("{file}/{}", v["id"].as_str().unwrap());
            let steps = v["input"]["steps"].as_array().unwrap();
            let outcomes = v["keripy"]["steps"].as_array().unwrap();
            let mut validator = View::default();
            let why = v["why"].as_str().unwrap_or_default();
            // Superseding recovery: keripy replaces an accepted event, the crate keeps it.
            let supersedes: Option<usize> = why
                .contains("superseding recovery")
                .then(|| {
                    why.trim_start_matches("step ")
                        .split(':')
                        .next()?
                        .parse()
                        .ok()
                })
                .flatten();
            let same = v["expect"] == "same" || supersedes.is_some();
            if v["why"]
                .as_str()
                .is_some_and(|why| why.contains("outside:"))
            {
                // A rule of keripy's the crate does not take: the event's source.
                replayed[2] += 1;
                continue;
            }
            let mut unsupported = false;
            for (n, (step, keripy)) in steps.iter().zip(outcomes).enumerate() {
                let message = match step["message"].as_str() {
                    Some(raw) => Message::read(raw.as_bytes(), &LIMITS).map(|(m, _)| m),
                    None => assembled(step),
                };
                let read = message.is_ok();
                let verdict = match message {
                    Ok(message) => validator.give(&message, checked_by(step).unwrap()),
                    Err(refusal) => Err(Verdict::Refused(refusal)),
                };
                if matches!(verdict, Err(Verdict::Refused(Refusal::Unsupported(_)))) {
                    unsupported = true;
                    break;
                }
                if !same {
                    continue;
                }
                if supersedes == Some(n) {
                    assert!(
                        matches!(verdict, Err(Verdict::LikelyDuplicitous { .. })),
                        "{id} step {n}: a superseding rotation was answered {verdict:?}"
                    );
                    break;
                }
                let outcome = keripy["outcome"].as_str().unwrap();
                if outcome == "ParseError" {
                    assert!(
                        !read,
                        "{id} step {n}: read, though keripy refuses the event as it reads it: {verdict:?}"
                    );
                    refused_as_read += 1;
                }
                assert_eq!(
                    answer(&verdict),
                    keripy_answer(outcome),
                    "{id} step {n}: {verdict:?}"
                );
                if verdict.is_ok() {
                    let prefix = keripy["state"]["i"].as_str().unwrap();
                    let ours = state(&validator.kels[prefix].0);
                    assert_eq!(
                        ours,
                        keripy_state(&keripy["state"]),
                        "{id} step {n}: key state"
                    );
                }
            }
            if same && supersedes.is_none() {
                refolded += refold(&id, &validator).unwrap();
            }
            if supersedes.is_some() {
                replayed[2] += 1;
            } else if same {
                assert!(
                    !unsupported,
                    "{id}: a KEL of the subset met an unsupported refusal"
                );
                replayed[0] += 1;
            } else {
                assert!(
                    unsupported,
                    "{id}: a KEL outside the subset met no unsupported refusal"
                );
                replayed[1] += 1;
            }
        }
    }
    assert!(
        replayed[0] > 0 && replayed[1] > 0,
        "same, unsupported, outside: {replayed:?}"
    );
    assert!(refolded > 0, "no KEL refolded");
    assert!(
        refused_as_read > 0,
        "no event keripy refuses as it reads it"
    );
}

/// Folding every AID's accepted messages again from the first reaches the key state accepting
/// them one by one reached: through `fold` where each was checked by a remote validator, and
/// through acceptance checked as each was where the validator is the AID's controller or
/// witness.
fn refold(id: &str, validator: &View) -> Result<usize, String> {
    validator
        .accepted
        .iter()
        .map(|(prefix, accepted)| {
            let (state, events) = validator
                .kels
                .get(prefix)
                .ok_or_else(|| format!("{id}: {prefix} accepted without a key state"))?;
            let delegator = state
                .delegator()
                .and_then(|d| validator.kels.get(d.to_text()))
                .map_or(&[][..], |(_, e)| e.as_slice());
            let folded = if accepted.iter().all(|(_, v)| *v == Validator::Remote) {
                fold(accepted.iter().map(|(m, _)| m), delegator)
            } else {
                accepted
                    .iter()
                    .try_fold(
                        (None, Vec::new()),
                        |(state, mut events), (message, checked)| {
                            let context = Context {
                                accepted: &events,
                                delegator,
                                validator: *checked,
                            };
                            let next = KeyState::accept(state.as_ref(), message, context)?;
                            events.push(message.event().clone());
                            Ok::<_, Verdict>((Some(next), events))
                        },
                    )
                    .and_then(|(state, events)| {
                        state
                            .map(|s| (s, events))
                            .ok_or_else(|| Verdict::Refused(Refusal::Invalid("empty".to_owned())))
                    })
            };
            let (folded, folded_events) =
                folded.map_err(|verdict| format!("{id}: {prefix} refolded to {verdict:?}"))?;
            assert_eq!(&folded, state, "{id}: {prefix} refolded");
            assert_eq!(&folded_events, events, "{id}: {prefix} events refolded");
            Ok(1)
        })
        .sum()
}

/// A step keripy wrote no message for — a receipt by a transferable AID, which its writer refuses
/// — assembled from the event and attachments it gave its validator.
fn assembled(step: &Value) -> Result<Message, Refusal> {
    let event = step["event"].as_str().unwrap_or_default();
    let (event, _) = Event::read(event.as_bytes(), &LIMITS)?;
    let texts = |field: &str| -> Vec<String> {
        step[field]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut message = Message::new(event);
    for signature in texts("sigs") {
        message = message.with_signature(IndexedSignature::read(&signature)?);
    }
    for receipt in texts("wigs") {
        message = message.witnessed(IndexedSignature::read(&receipt)?);
    }
    let delegation = step.get("delegation");
    let field = |f: &str| delegation.and_then(|d| d.get(f)).and_then(Value::as_str);
    if let (Some(sn), Some(said)) = (field("s"), field("d")) {
        let sn = u128::from_str_radix(sn, 16).map_err(|e| Refusal::Malformed(e.to_string()))?;
        message = message.sourced(Source {
            sn,
            said: Primitive::read(said)?,
        });
    }
    Ok(message)
}
