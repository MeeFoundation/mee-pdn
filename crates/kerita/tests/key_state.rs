//! Key state on the crate's own KEL: what an accepted event leaves for the layers above, and
//! events no KEL of the corpus carries.

mod support;

#[path = "support/crate_kels.rs"]
mod crate_kels;

use kerita::event::Limits;
use kerita::message::Message;
use kerita::primitive::{IndexedSignature, Refusal};
use kerita::state::{Context, KeyState, Slot, Verdict, fold};

const LIMITS: Limits = Limits {
    body: 16_384,
    list: 384,
    attachments: 4_096,
};

/// Alice's identity KEL as the crate writes it, its own events alone: inception, rotations 1 to
/// 3 and rotation 4 receipted by the witness it adds.
fn alice() -> Result<Vec<Message>, String> {
    let case = crate_kels::cases()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|c| c.name == "alice")
        .ok_or("no case alice")?;
    let messages: Vec<Message> = case
        .steps
        .into_iter()
        .filter_map(crate_kels::Step::message)
        .collect();
    let identity = messages
        .first()
        .ok_or("no inception")?
        .event()
        .prefix()
        .clone();
    let own: Vec<Message> = messages
        .into_iter()
        .filter(|m| m.event().prefix() == &identity)
        .collect();
    // Inception, rotations 1 and 2, rotation 3 with both signers, rotation 4 by W-home.
    [0, 1, 2, 4, 6]
        .iter()
        .map(|&n| own.get(n).cloned().ok_or_else(|| format!("no message {n}")))
        .collect()
}

fn folded(messages: &[Message]) -> Result<KeyState, Verdict> {
    fold(messages, &[]).map(|(state, _)| state)
}

/// An accepted event names each key that signed it once, with the prior next-key slot a rotation's
/// signature exposed — and the slot it exposed alone, though a relay attaches copies re-encoded to
/// claim other slots: rotation 4's laptop signature claiming the paper's slot 2 claims nothing.
#[test]
fn an_accepted_event_names_each_signing_slot_once_with_what_it_exposed() {
    let kel = alice().unwrap();
    assert_eq!(
        folded(&kel[..1]).unwrap().signers(),
        [Slot {
            index: 0,
            exposed: None
        }],
        "an inception exposes no slot"
    );
    assert_eq!(
        folded(&kel[..4]).unwrap().signers(),
        [
            Slot {
                index: 0,
                exposed: Some(2)
            },
            Slot {
                index: 1,
                exposed: Some(0)
            }
        ],
        "rotation 3: the laptop at the slot it was committed in, the phone at its own"
    );

    let rotation = kel[4].clone();
    assert_eq!(
        rotation.signatures().len(),
        2,
        "the laptop's and the phone's"
    );
    let (laptop, phone) = (&rotation.signatures()[0], &rotation.signatures()[1]);
    let key = rotation.event().keys()[0].clone();
    let claim = IndexedSignature::new(&key, 0, Some(2), laptop.raw()).unwrap();
    // The relay puts its copy first, so the genuine signature at that index comes after it.
    let relayed = rotation
        .witness_signatures()
        .iter()
        .fold(Message::new(rotation.event().clone()), |m, w| {
            m.witnessed(w.clone())
        })
        .with_signature(claim)
        .with_signature(laptop.clone())
        .with_signature(phone.clone())
        .with_signature(phone.clone());
    let mut kel = kel;
    kel[4] = relayed;
    assert_eq!(
        folded(&kel).unwrap().signers(),
        [
            Slot {
                index: 0,
                exposed: Some(0)
            },
            Slot {
                index: 1,
                exposed: Some(1)
            }
        ]
    );
}

/// An event naming another AID than the key state it is given to is refused, though it follows
/// that key state's latest event and is signed by the key that event committed to.
#[test]
fn an_event_of_another_aid_is_refused() {
    let (icp, foreign) = crate_kels::foreign_rotation().unwrap();
    let (state, events) = fold([&icp], &[]).unwrap();
    let verdict = KeyState::accept(
        Some(&state),
        &foreign,
        Context {
            accepted: &events,
            ..Context::default()
        },
    );
    assert!(
        matches!(verdict, Err(Verdict::Refused(Refusal::Invalid(_)))),
        "{verdict:?}"
    );
}

/// A receipt given as an event of a KEL is refused as invalid: the receipt is of the subset, its
/// place is not a KEL.
#[test]
fn a_receipt_given_as_an_event_is_refused() {
    let receipt = "-FAb0OKERICAACAAXrctENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4ENcf5ii15MsVCx7wxKi4muIodYJULhkOaEbvbz4cF3d4MAAA";
    let (message, _) = Message::read(receipt.as_bytes(), &LIMITS).unwrap();
    let verdict = KeyState::accept(None, &message, Context::default());
    assert!(
        matches!(verdict, Err(Verdict::Refused(Refusal::Invalid(_)))),
        "{verdict:?}"
    );
}
