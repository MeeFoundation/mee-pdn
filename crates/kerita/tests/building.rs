//! Events the crate builds: they read back as themselves, their SAID and self-addressing prefix
//! are checked, and a signature made outside the crate is attached only when it verifies.

use kerita::event::{Draft, Event, Ilk, Limits, Seal};
use kerita::message::Message;
use kerita::primitive::{IndexedSignature, Primitive, Refusal};
use kerita::threshold::{Threshold, Weight};
use p256::ecdsa::signature::Signer as _;
use proptest::prelude::*;

const LIMITS: Limits = Limits {
    body: 16_384,
    list: 384,
    attachments: 4_096,
};

fn p256(seed: u8) -> p256::ecdsa::SigningKey {
    // A scalar of 32 equal non-zero bytes is below the group's order.
    p256::ecdsa::SigningKey::from_slice(&[seed.max(1); 32]).unwrap_or_else(|_| unreachable!())
}

fn public(key: &p256::ecdsa::SigningKey) -> Result<Primitive, Refusal> {
    Primitive::new("1AAJ", key.verifying_key().to_sec1_point(true).as_bytes())
}

fn digest(of: &Primitive) -> Result<Primitive, Refusal> {
    Primitive::new("E", blake3::hash(of.to_text().as_bytes()).as_bytes())
}

fn inception(keys: &[p256::ecdsa::SigningKey]) -> Result<Event, Refusal> {
    let publics = keys.iter().map(public).collect::<Result<Vec<_>, _>>()?;
    Event::build(
        Ilk::Inception,
        Draft {
            key_threshold: Some(Threshold::Count(1)),
            keys: publics.clone(),
            next_threshold: Some(Threshold::Count(1)),
            next_digests: publics.iter().map(digest).collect::<Result<_, _>>()?,
            traits: vec!["EO".to_owned()],
            ..Draft::default()
        },
    )
}

proptest! {
    /// Any inception the crate builds — keys, thresholds, witnesses, seals — reads back from its
    /// text as the same event, its prefix its SAID.
    #[test]
    fn a_built_event_reads_back_as_itself(
        seeds in proptest::collection::vec(1u8..200, 1..5),
        weighted in any::<bool>(),
        witnesses in 0usize..3,
        sealed in any::<bool>(),
    ) {
        let keys: Vec<_> = seeds.iter().map(|&s| public(&p256(s)).unwrap()).collect();
        let threshold = if weighted {
            let share = Weight::new(1, u64::try_from(keys.len()).unwrap()).unwrap();
            Threshold::weighted(vec![keys.iter().map(|_| share).collect()]).unwrap()
        } else {
            Threshold::Count(1)
        };
        let witnesses: Vec<_> = (0..witnesses).map(|w| Primitive::new("B", &[u8::try_from(w).unwrap() + 1; 32]).unwrap()).collect();
        let seals = if sealed { vec![Seal::Digest(digest(&keys[0]).unwrap()), Seal::Event { prefix: digest(&keys[0]).unwrap(), sn: 3, said: digest(&keys[0]).unwrap() }] } else { vec![] };
        let event = Event::build(Ilk::Inception, Draft {
            key_threshold: Some(threshold.clone()),
            keys: keys.clone(),
            next_threshold: Some(threshold),
            next_digests: keys.iter().map(|k| digest(k).unwrap()).collect(),
            witness_threshold: u128::try_from(witnesses.len()).unwrap(),
            witnesses,
            seals,
            ..Draft::default()
        }).unwrap();
        prop_assert_eq!(event.prefix(), event.said());
        let (read, used) = Event::read(event.text().as_bytes(), &LIMITS).unwrap();
        prop_assert_eq!(used, event.text().len());
        prop_assert_eq!(&read, &event);
        prop_assert_eq!(read.to_text().unwrap(), event.text());
    }
}

/// An inception whose self-addressing prefix is not its SAID is refused, as read and as built.
#[test]
fn a_self_addressing_prefix_that_is_not_the_said_is_refused() {
    let key = public(&p256(1)).unwrap();
    let other = digest(&key).unwrap();
    let event = inception(&[p256(1)]).unwrap();
    let text = event.text();
    // The prefix follows the SAID, which it equals.
    let at = text.rfind(event.prefix().to_text()).unwrap();
    let forged = format!("{}{}{}", &text[..at], other.to_text(), &text[at + 44..]);
    assert!(matches!(
        Event::read(forged.as_bytes(), &LIMITS),
        Err(Refusal::Malformed(_))
    ));
    let built = Event::build(
        Ilk::Inception,
        Draft {
            prefix: Some(other.clone()),
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![key],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![other],
            ..Draft::default()
        },
    );
    assert!(matches!(built, Err(Refusal::Invalid(_))), "{built:?}");
}

/// The draft of each event type that carries its fields alone.
fn draft(ilk: Ilk, keys: &[Primitive], aid: &Primitive) -> Result<Draft, Refusal> {
    let threshold = Some(Threshold::Count(1));
    let establishment = Draft {
        key_threshold: threshold.clone(),
        keys: keys.to_vec(),
        next_threshold: threshold,
        next_digests: keys.iter().map(digest).collect::<Result<_, _>>()?,
        ..Draft::default()
    };
    let following = |draft: Draft| Draft {
        prefix: Some(aid.clone()),
        sn: 1,
        prior: Some(aid.clone()),
        ..draft
    };
    Ok(match ilk {
        Ilk::Inception => establishment,
        Ilk::DelegatedInception => Draft {
            delegator: Some(aid.clone()),
            ..establishment
        },
        Ilk::Rotation | Ilk::DelegatedRotation => following(establishment),
        Ilk::Interaction | Ilk::Receipt => following(Draft::default()),
    })
}

proptest! {
    /// An event of every type the crate builds from a draft holding that type's fields reads
    /// back from its text as the same event.
    #[test]
    fn every_event_type_built_reads_back_as_itself(
        ilk in proptest::sample::select(vec![
            Ilk::Inception, Ilk::Rotation, Ilk::Interaction,
            Ilk::DelegatedInception, Ilk::DelegatedRotation,
        ]),
        seeds in proptest::collection::vec(1u8..200, 1..4),
        sealed in any::<bool>(),
    ) {
        let keys: Vec<_> = seeds.iter().map(|&s| public(&p256(s)).unwrap()).collect();
        let aid = digest(&keys[0]).unwrap();
        let seals = if sealed { vec![Seal::Digest(aid.clone())] } else { vec![] };
        let event = Event::build(ilk, Draft { seals, ..draft(ilk, &keys, &aid).unwrap() }).unwrap();
        let (read, used) = Event::read(event.text().as_bytes(), &LIMITS).unwrap();
        prop_assert_eq!(used, event.text().len());
        prop_assert_eq!(&read, &event);
    }
}

/// A draft carrying a field its event type does not carry is refused, rather than built into an
/// event whose fields differ from its text; so is a draft of a receipt, which names no event.
#[test]
fn a_draft_field_the_event_type_does_not_carry_is_refused() {
    let keys = vec![public(&p256(3)).unwrap()];
    let aid = digest(&keys[0]).unwrap();
    let cases = [
        (
            "a delegated inception without its delegator",
            Ilk::DelegatedInception,
            Draft {
                delegator: None,
                ..draft(Ilk::DelegatedInception, &keys, &aid).unwrap()
            },
        ),
        (
            "an inception naming a delegator",
            Ilk::Inception,
            Draft {
                delegator: Some(aid.clone()),
                ..draft(Ilk::Inception, &keys, &aid).unwrap()
            },
        ),
        (
            "an inception with a prior event",
            Ilk::Inception,
            Draft {
                prior: Some(aid.clone()),
                ..draft(Ilk::Inception, &keys, &aid).unwrap()
            },
        ),
        (
            "a delegated rotation naming a delegator",
            Ilk::DelegatedRotation,
            Draft {
                delegator: Some(aid.clone()),
                ..draft(Ilk::DelegatedRotation, &keys, &aid).unwrap()
            },
        ),
        (
            "a rotation with an inception's witness list",
            Ilk::Rotation,
            Draft {
                witnesses: vec![Primitive::new("B", &[9; 32]).unwrap()],
                ..draft(Ilk::Rotation, &keys, &aid).unwrap()
            },
        ),
        (
            "an interaction event with keys",
            Ilk::Interaction,
            Draft {
                keys: keys.clone(),
                ..draft(Ilk::Interaction, &keys, &aid).unwrap()
            },
        ),
        (
            "a receipt",
            Ilk::Receipt,
            Draft {
                prefix: Some(aid.clone()),
                ..Draft::default()
            },
        ),
    ];
    for (case, ilk, draft) in cases {
        let built = Event::build(ilk, draft);
        assert!(
            matches!(built, Err(Refusal::Invalid(_))),
            "{case}: {built:?}"
        );
    }
}

/// A signature made outside the crate over the bytes the crate gives is attached; one over
/// other bytes is refused.
#[test]
fn a_signature_made_outside_the_crate_is_attached() {
    let key = p256(7);
    let event = inception(std::slice::from_ref(&key)).unwrap();
    let signed: p256::ecdsa::Signature = key.sign(event.text().as_bytes());
    let signature =
        IndexedSignature::new(&public(&key).unwrap(), 0, Some(0), &signed.to_bytes()).unwrap();
    let message = Message::new(event.clone())
        .signed(&public(&key).unwrap(), signature)
        .unwrap();
    assert_eq!(message.signatures().len(), 1);

    let other: p256::ecdsa::Signature = key.sign(b"other bytes");
    let forged =
        IndexedSignature::new(&public(&key).unwrap(), 0, Some(0), &other.to_bytes()).unwrap();
    assert!(
        Message::new(event)
            .signed(&public(&key).unwrap(), forged)
            .is_err()
    );
}
