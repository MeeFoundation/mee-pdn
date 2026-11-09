//! The KELs the crate writes for keripy to fold: Alice's identity on P-256 keys — establishment
//! only, weighted thresholds, a dual-indexed partial rotation, device AIDs approved by key event
//! seals in rotations, a change of witness — with the steps keripy must hold back; then one KEL
//! for each rule of the key state that refuses an event, the refused event beside one keripy
//! accepts. Seeds are fixed and ECDSA signing is deterministic (RFC 6979), so the bytes are the
//! same on every run.
// Each test binary uses part of this module.
#![allow(dead_code)]

use ed25519_dalek::Signer as _;
use kerita::event::{Draft, Event, Ilk, Seal};
use kerita::message::{Message, Source};
use kerita::primitive::{IndexedSignature, Primitive, Refusal};
use kerita::threshold::{Threshold, Weight};

pub struct Case {
    pub name: &'static str,
    pub messages: Vec<Message>,
}

struct Key(p256::ecdsa::SigningKey);

impl Key {
    fn new(name: &str) -> Result<Self, Refusal> {
        let seed = blake3::hash(name.as_bytes());
        p256::ecdsa::SigningKey::from_slice(seed.as_bytes())
            .map(Self)
            .map_err(|e| Refusal::Malformed(e.to_string()))
    }

    fn public(&self) -> Result<Primitive, Refusal> {
        self.coded("1AAJ")
    }

    /// The key under `code`: `1AAJ` transferable, `1AAI` not.
    fn coded(&self, code: &str) -> Result<Primitive, Refusal> {
        Primitive::new(code, self.0.verifying_key().to_sec1_point(true).as_bytes())
    }

    fn digest(&self) -> Result<Primitive, Refusal> {
        Primitive::new(
            "E",
            blake3::hash(self.public()?.to_text().as_bytes()).as_bytes(),
        )
    }
}

struct Witness(ed25519_dalek::SigningKey);

impl Witness {
    fn new(name: &str) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(
            blake3::hash(name.as_bytes()).as_bytes(),
        ))
    }

    fn prefix(&self) -> Result<Primitive, Refusal> {
        Primitive::new("B", self.0.verifying_key().as_bytes())
    }

    fn receipt(&self, event: &Event, index: u32) -> Result<IndexedSignature, Refusal> {
        let signature = self.0.sign(event.text().as_bytes());
        IndexedSignature::new(&self.prefix()?, index, Some(index), &signature.to_bytes())
    }
}

fn keys(name: &str, count: usize) -> Result<Vec<Key>, Refusal> {
    (0..count)
        .map(|n| Key::new(&format!("{name}-{n}")))
        .collect()
}

fn weights(fractions: &[(u64, u64)]) -> Result<Threshold, Refusal> {
    let clause = fractions
        .iter()
        .map(|&(n, d)| Weight::new(n, d))
        .collect::<Result<_, _>>()?;
    Threshold::weighted(vec![clause])
}

fn signed(event: &Event, signers: &[(&Key, u32, Option<u32>)]) -> Result<Message, Refusal> {
    signers.iter().try_fold(
        Message::new(event.clone()),
        |message, (key, index, ondex)| {
            let signature: p256::ecdsa::Signature = key.0.sign(event.text().as_bytes());
            let public = key.public()?;
            let indexed = IndexedSignature::new(&public, *index, *ondex, &signature.to_bytes())?;
            message.signed(&public, indexed)
        },
    )
}

fn device(name: &str, delegator: &Primitive) -> Result<(Event, Key), Refusal> {
    let [first, next]: [Key; 2] = keys(name, 2)?
        .try_into()
        .map_err(|_| Refusal::Malformed("two keys".to_owned()))?;
    let dip = Event::build(
        Ilk::DelegatedInception,
        Draft {
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![first.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![next.digest()?],
            delegator: Some(delegator.clone()),
            ..Draft::default()
        },
    )?;
    Ok((dip, first))
}

fn seal(event: &Event) -> Seal {
    Seal::Event {
        prefix: event.prefix().clone(),
        sn: event.sn(),
        said: event.said().clone(),
    }
}

fn source(event: &Event) -> Source {
    Source {
        sn: event.sn(),
        said: event.said().clone(),
    }
}

/// Key `n` of a list the case builds with enough keys.
fn at(keys: &[Key], n: usize) -> Result<&Key, Refusal> {
    keys.get(n)
        .ok_or_else(|| Refusal::Malformed(format!("no key {n}")))
}

#[allow(
    clippy::too_many_lines,
    reason = "one KEL, step by step, reads best whole"
)]
fn alice() -> Result<Case, Refusal> {
    let phone = keys("alice-phone", 6)?;
    let paper = keys("alice-paper", 2)?;
    let laptop = keys("alice-laptop", 3)?;
    let (tellipod, home) = (Witness::new("w-tellipod"), Witness::new("w-home"));
    let witnesses = vec![tellipod.prefix()?];

    let icp = Event::build(
        Ilk::Inception,
        Draft {
            key_threshold: Some(weights(&[(1, 1), (1, 1)])?),
            keys: vec![at(&phone, 0)?.public()?, at(&paper, 0)?.public()?],
            next_threshold: Some(weights(&[(1, 1), (1, 1)])?),
            next_digests: vec![at(&phone, 1)?.digest()?, at(&paper, 1)?.digest()?],
            witness_threshold: 1,
            witnesses: witnesses.clone(),
            traits: vec!["EO".to_owned()],
            ..Draft::default()
        },
    )?;
    let alice = icp.prefix().clone();
    let (phone_dip, phone_device) = device("alice-phone-device", &alice)?;
    let (laptop_dip, laptop_device) = device("alice-laptop-device", &alice)?;
    let rotation = |sn: u128, prior: &Event, draft: Draft| {
        Event::build(
            Ilk::Rotation,
            Draft {
                prefix: Some(alice.clone()),
                sn,
                prior: Some(prior.said().clone()),
                witness_threshold: 1,
                ..draft
            },
        )
    };
    let rot1 = rotation(
        1,
        &icp,
        Draft {
            key_threshold: Some(weights(&[(1, 1)])?),
            keys: vec![at(&phone, 1)?.public()?],
            next_threshold: Some(weights(&[(1, 1), (1, 1)])?),
            next_digests: vec![at(&phone, 2)?.digest()?, at(&paper, 1)?.digest()?],
            seals: vec![seal(&phone_dip)],
            ..Draft::default()
        },
    )?;
    let rot2 = rotation(
        2,
        &rot1,
        Draft {
            key_threshold: Some(weights(&[(1, 1)])?),
            keys: vec![at(&phone, 2)?.public()?],
            next_threshold: Some(weights(&[(1, 2), (1, 1), (1, 2)])?),
            next_digests: vec![
                at(&phone, 3)?.digest()?,
                at(&paper, 1)?.digest()?,
                at(&laptop, 0)?.digest()?,
            ],
            seals: vec![seal(&laptop_dip)],
            ..Draft::default()
        },
    )?;
    let rot3 = rotation(
        3,
        &rot2,
        Draft {
            key_threshold: Some(weights(&[(1, 2), (1, 2)])?),
            keys: vec![at(&laptop, 0)?.public()?, at(&phone, 3)?.public()?],
            next_threshold: Some(weights(&[(1, 2), (1, 2), (1, 1)])?),
            next_digests: vec![
                at(&laptop, 1)?.digest()?,
                at(&phone, 4)?.digest()?,
                at(&paper, 1)?.digest()?,
            ],
            ..Draft::default()
        },
    )?;
    let rot4 = rotation(
        4,
        &rot3,
        Draft {
            key_threshold: Some(weights(&[(1, 2), (1, 2)])?),
            keys: vec![at(&laptop, 1)?.public()?, at(&phone, 4)?.public()?],
            next_threshold: Some(weights(&[(1, 2), (1, 2), (1, 1)])?),
            next_digests: vec![
                at(&laptop, 2)?.digest()?,
                at(&phone, 5)?.digest()?,
                at(&paper, 1)?.digest()?,
            ],
            cuts: witnesses,
            adds: vec![home.prefix()?],
            ..Draft::default()
        },
    )?;

    let both = [(at(&laptop, 1)?, 0, Some(0)), (at(&phone, 4)?, 1, Some(1))];
    Ok(Case {
        name: "alice",
        messages: vec![
            signed(&icp, &[(at(&phone, 0)?, 0, Some(0))])?.witnessed(tellipod.receipt(&icp, 0)?),
            signed(&rot1, &[(at(&phone, 1)?, 0, Some(0))])?.witnessed(tellipod.receipt(&rot1, 0)?),
            signed(&phone_dip, &[(&phone_device, 0, Some(0))])?.sourced(source(&rot1)),
            signed(&rot2, &[(at(&phone, 2)?, 0, Some(0))])?.witnessed(tellipod.receipt(&rot2, 0)?),
            signed(&laptop_dip, &[(&laptop_device, 0, Some(0))])?.sourced(source(&rot2)),
            // One signer short: the laptop alone exposes 1/2 of the prior next threshold.
            signed(&rot3, &[(at(&laptop, 0)?, 0, Some(2))])?.witnessed(tellipod.receipt(&rot3, 0)?),
            signed(
                &rot3,
                &[(at(&laptop, 0)?, 0, Some(2)), (at(&phone, 3)?, 1, Some(0))],
            )?
            .witnessed(tellipod.receipt(&rot3, 0)?),
            // The receipt of the witness rotation 4 cuts counts for nothing.
            signed(&rot4, &both)?.witnessed(tellipod.receipt(&rot4, 0)?),
            signed(&rot4, &both)?.witnessed(home.receipt(&rot4, 0)?),
        ],
    })
}

/// An inception of `key` alone, committing to `next`, with what `change` sets on top.
fn incepted(key: &Key, next: &Key, change: impl FnOnce(Draft) -> Draft) -> Result<Event, Refusal> {
    Event::build(
        Ilk::Inception,
        change(Draft {
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![key.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![next.digest()?],
            ..Draft::default()
        }),
    )
}

/// A rotation of `prior`'s AID to `key`, committing to `next`, with what `change` sets on top.
fn rotated(
    ilk: Ilk,
    prior: &Event,
    key: &Key,
    next: &Key,
    change: impl FnOnce(Draft) -> Draft,
) -> Result<Event, Refusal> {
    Event::build(
        ilk,
        change(Draft {
            prefix: Some(prior.prefix().clone()),
            sn: prior.sn() + 1,
            prior: Some(prior.said().clone()),
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![key.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![next.digest()?],
            ..Draft::default()
        }),
    )
}

/// A basic prefix is its inception's one key: another key under it is refused, and the key's own
/// inception accepted.
fn basic_prefix() -> Result<Case, Refusal> {
    let keys = keys("basic", 4)?;
    let prefix = at(&keys, 0)?.public()?;
    let forged = incepted(at(&keys, 2)?, at(&keys, 3)?, |d| Draft {
        prefix: Some(prefix.clone()),
        ..d
    })?;
    let own = incepted(at(&keys, 0)?, at(&keys, 1)?, |d| Draft {
        prefix: Some(prefix.clone()),
        ..d
    })?;
    Ok(Case {
        name: "basic-prefix",
        messages: vec![
            signed(&forged, &[(at(&keys, 2)?, 0, Some(0))])?,
            signed(&own, &[(at(&keys, 0)?, 0, Some(0))])?,
        ],
    })
}

/// A non-transferable AID commits to no next key: one that does is refused, one that does not is
/// accepted, and a rotation of it is refused.
fn non_transferable() -> Result<Case, Refusal> {
    let keys = keys("non-transferable", 3)?;
    let owner = at(&keys, 0)?;
    let prefix = owner.coded("1AAI")?;
    let committing = incepted(owner, at(&keys, 1)?, |d| Draft {
        prefix: Some(prefix.clone()),
        keys: vec![prefix.clone()],
        ..d
    })?;
    let icp = incepted(owner, at(&keys, 1)?, |d| Draft {
        prefix: Some(prefix.clone()),
        keys: vec![prefix.clone()],
        next_threshold: Some(Threshold::Count(0)),
        next_digests: vec![],
        ..d
    })?;
    let thief = Key::new("thief")?;
    let rot = rotated(Ilk::Rotation, &icp, &thief, at(&keys, 2)?, |d| d)?;
    Ok(Case {
        name: "non-transferable",
        messages: vec![
            signed(&committing, &[(owner, 0, None)])?,
            signed(&icp, &[(owner, 0, None)])?,
            signed(&rot, &[(&thief, 0, None)])?,
        ],
    })
}

/// A rotation names the latest event's SAID as its prior: one naming another is refused, the one
/// naming it accepted.
fn prior_digest() -> Result<Case, Refusal> {
    let keys = keys("prior", 3)?;
    let icp = incepted(at(&keys, 0)?, at(&keys, 1)?, |d| d)?;
    let other = at(&keys, 2)?.digest()?;
    let elsewhere = rotated(Ilk::Rotation, &icp, at(&keys, 1)?, at(&keys, 2)?, |d| {
        Draft {
            prior: Some(other),
            ..d
        }
    })?;
    let rot = rotated(Ilk::Rotation, &icp, at(&keys, 1)?, at(&keys, 2)?, |d| d)?;
    Ok(Case {
        name: "prior-digest",
        messages: vec![
            signed(&icp, &[(at(&keys, 0)?, 0, Some(0))])?,
            signed(&elsewhere, &[(at(&keys, 1)?, 0, Some(0))])?,
            signed(&rot, &[(at(&keys, 1)?, 0, Some(0))])?,
        ],
    })
}

/// A witness list holds no witness twice, a rotation cuts listed witnesses and adds unlisted
/// ones, and the threshold fits the list: each broken is refused, the event keeping them
/// accepted.
fn witness_lists() -> Result<Case, Refusal> {
    let keys = keys("witness-lists", 3)?;
    let one = Witness::new("w-one");
    let (first, second) = (one.prefix()?, Witness::new("w-two").prefix()?);
    let listed = |witnesses: Vec<Primitive>| {
        move |d: Draft| Draft {
            witness_threshold: 1,
            witnesses,
            ..d
        }
    };
    let twice = incepted(
        at(&keys, 0)?,
        at(&keys, 1)?,
        listed(vec![first.clone(), first.clone()]),
    )?;
    let icp = incepted(at(&keys, 0)?, at(&keys, 1)?, listed(vec![first.clone()]))?;
    let next = |change: Draft| {
        rotated(Ilk::Rotation, &icp, at(&keys, 1)?, at(&keys, 2)?, |d| {
            Draft {
                witness_threshold: change.witness_threshold,
                cuts: change.cuts,
                adds: change.adds,
                ..d
            }
        })
    };
    let cut_unlisted = next(Draft {
        witness_threshold: 1,
        cuts: vec![second],
        ..Draft::default()
    })?;
    let add_listed = next(Draft {
        witness_threshold: 1,
        adds: vec![first],
        ..Draft::default()
    })?;
    let above = next(Draft {
        witness_threshold: 2,
        ..Draft::default()
    })?;
    let rot = next(Draft {
        witness_threshold: 1,
        ..Draft::default()
    })?;
    let rotation = |event: &Event| -> Result<Message, Refusal> {
        Ok(signed(event, &[(at(&keys, 1)?, 0, Some(0))])?.witnessed(one.receipt(event, 0)?))
    };
    Ok(Case {
        name: "witness-lists",
        messages: vec![
            signed(&twice, &[(at(&keys, 0)?, 0, Some(0))])?.witnessed(one.receipt(&twice, 0)?),
            signed(&icp, &[(at(&keys, 0)?, 0, Some(0))])?.witnessed(one.receipt(&icp, 0)?),
            rotation(&cut_unlisted)?,
            rotation(&add_listed)?,
            rotation(&above)?,
            rotation(&rot)?,
        ],
    })
}

/// A count threshold weighs no more keys than the event lists: one over 2 keys asking for 3 is
/// refused, one asking for 2 accepted.
fn threshold_over_keys() -> Result<Case, Refusal> {
    let keys = keys("threshold-over", 4)?;
    let listed = vec![at(&keys, 0)?.public()?, at(&keys, 1)?.public()?];
    let two = |n: u128| {
        incepted(at(&keys, 0)?, at(&keys, 2)?, |d| Draft {
            key_threshold: Some(Threshold::Count(n)),
            keys: listed.clone(),
            ..d
        })
    };
    let over = two(3)?;
    let icp = two(2)?;
    let both = |event: &Event| {
        signed(
            event,
            &[(at(&keys, 0)?, 0, Some(0)), (at(&keys, 1)?, 1, None)],
        )
    };
    Ok(Case {
        name: "threshold-over-keys",
        messages: vec![both(&over)?, both(&icp)?],
    })
}

/// A delegated AID rotates by delegated rotations alone: its plain rotation is refused. An AID
/// nothing delegates may rotate by either, as keripy accepts it: its delegated rotation needs no
/// approval.
fn rotation_kind() -> Result<Case, Refusal> {
    let devices = keys("rotation-kind-device", 3)?;
    let keys = keys("rotation-kind", 3)?;
    let icp = incepted(at(&keys, 0)?, at(&keys, 1)?, |d| d)?;
    let dip = Event::build(
        Ilk::DelegatedInception,
        Draft {
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![at(&devices, 0)?.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![at(&devices, 1)?.digest()?],
            delegator: Some(icp.prefix().clone()),
            ..Draft::default()
        },
    )?;
    let approval = Event::build(
        Ilk::Interaction,
        Draft {
            prefix: Some(icp.prefix().clone()),
            sn: 1,
            prior: Some(icp.said().clone()),
            seals: vec![seal(&dip)],
            ..Draft::default()
        },
    )?;
    let plain = rotated(
        Ilk::Rotation,
        &dip,
        at(&devices, 1)?,
        at(&devices, 2)?,
        |d| d,
    )?;
    let drt = rotated(
        Ilk::DelegatedRotation,
        &approval,
        at(&keys, 1)?,
        at(&keys, 2)?,
        |d| d,
    )?;
    Ok(Case {
        name: "rotation-kind",
        messages: vec![
            signed(&icp, &[(at(&keys, 0)?, 0, Some(0))])?,
            signed(&approval, &[(at(&keys, 0)?, 0, None)])?,
            signed(&dip, &[(at(&devices, 0)?, 0, Some(0))])?.sourced(source(&approval)),
            signed(&plain, &[(at(&devices, 1)?, 0, Some(0))])?,
            signed(&drt, &[(at(&keys, 1)?, 0, Some(0))])?,
        ],
    })
}

/// A delegated AID is self-addressing: a delegated inception under a basic prefix is refused.
fn delegated_basic_prefix() -> Result<Case, Refusal> {
    let keys = keys("delegated-basic", 3)?;
    let delegator = incepted(at(&keys, 2)?, at(&keys, 1)?, |d| d)?;
    let dip = Event::build(
        Ilk::DelegatedInception,
        Draft {
            prefix: Some(at(&keys, 0)?.public()?),
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![at(&keys, 0)?.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![at(&keys, 1)?.digest()?],
            delegator: Some(delegator.prefix().clone()),
            ..Draft::default()
        },
    )?;
    Ok(Case {
        name: "delegated-basic-prefix",
        messages: vec![signed(&dip, &[(at(&keys, 0)?, 0, Some(0))])?],
    })
}

/// An establishment-only KEL takes no interaction event, though its current key signs it.
fn establishment_only() -> Result<Case, Refusal> {
    let keys = keys("establishment-only", 2)?;
    let icp = incepted(at(&keys, 0)?, at(&keys, 1)?, |d| Draft {
        traits: vec!["EO".to_owned()],
        ..d
    })?;
    let ixn = Event::build(
        Ilk::Interaction,
        Draft {
            prefix: Some(icp.prefix().clone()),
            sn: 1,
            prior: Some(icp.said().clone()),
            ..Draft::default()
        },
    )?;
    Ok(Case {
        name: "establishment-only",
        messages: vec![
            signed(&icp, &[(at(&keys, 0)?, 0, Some(0))])?,
            signed(&ixn, &[(at(&keys, 0)?, 0, None)])?,
        ],
    })
}

/// Alice's inception, and a rotation that follows it — its prior that inception's SAID, signed by
/// the phone's committed next key, receipted by her witness — but names another AID.
pub fn foreign_rotation() -> Result<(Message, Message), Refusal> {
    let phone = keys("alice-phone", 3)?;
    let paper = keys("alice-paper", 2)?;
    let tellipod = Witness::new("w-tellipod");
    let icp = Event::build(
        Ilk::Inception,
        Draft {
            key_threshold: Some(weights(&[(1, 1), (1, 1)])?),
            keys: vec![at(&phone, 0)?.public()?, at(&paper, 0)?.public()?],
            next_threshold: Some(weights(&[(1, 1), (1, 1)])?),
            next_digests: vec![at(&phone, 1)?.digest()?, at(&paper, 1)?.digest()?],
            witness_threshold: 1,
            witnesses: vec![tellipod.prefix()?],
            traits: vec!["EO".to_owned()],
            ..Draft::default()
        },
    )?;
    let rot = Event::build(
        Ilk::Rotation,
        Draft {
            prefix: Some(at(&paper, 1)?.digest()?),
            sn: 1,
            prior: Some(icp.said().clone()),
            key_threshold: Some(Threshold::Count(1)),
            keys: vec![at(&phone, 1)?.public()?],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: vec![at(&phone, 2)?.digest()?],
            witness_threshold: 1,
            ..Draft::default()
        },
    )?;
    Ok((
        signed(&icp, &[(at(&phone, 0)?, 0, Some(0))])?.witnessed(tellipod.receipt(&icp, 0)?),
        signed(&rot, &[(at(&phone, 1)?, 0, Some(0))])?.witnessed(tellipod.receipt(&rot, 0)?),
    ))
}

pub fn cases() -> Result<Vec<Case>, Refusal> {
    Ok(vec![
        alice()?,
        basic_prefix()?,
        non_transferable()?,
        prior_digest()?,
        witness_lists()?,
        threshold_over_keys()?,
        rotation_kind()?,
        delegated_basic_prefix()?,
        establishment_only()?,
    ])
}
