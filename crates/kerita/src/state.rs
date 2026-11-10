//! Key state: a KEL folded event by event, and the verdict for an event that does not advance
//! it. The checks run in keripy's order, so the answer to an event is keripy's answer. Spec:
//! `kerita/key-state`.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::event::{Event, Ilk, Seal};
use crate::message::Message;
use crate::primitive::{IndexedSignature, Primitive, Refusal};
use crate::signature;
use crate::threshold::Threshold;

/// Why an event does not advance the key state; each answer is keripy's, named beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The event accepted at its position, again: keripy raises nothing. Signatures and receipts
    /// it carries beyond the accepted event's are the caller's to keep.
    Duplicate,
    /// Another event at a position already taken, its signatures not checked, so no proof of
    /// duplicity: keripy's `LikelyDuplicitousError`. The accepted event stays.
    LikelyDuplicitous {
        accepted: Primitive,
        rival: Box<Message>,
    },
    /// The event waits for the prior event at this sequence number: keripy's `OutOfOrderError`.
    AwaitingPrior { sn: u128 },
    /// The event waits for more signatures: keripy's `MissingSignatureError`.
    AwaitingSignatures,
    /// The event waits for more witness receipts: keripy's `MissingWitnessSignatureError`.
    AwaitingReceipts,
    /// The delegated event waits for its delegator to approve it: keripy's
    /// `MissingDelegationError`.
    AwaitingDelegator { delegator: Primitive },
    /// The event is refused: keripy's `ValidationError` and its kin, or a refusal to read it.
    Refused(Refusal),
}

impl From<Refusal> for Verdict {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// A key that signed an event, once per index into the event's keys: for a rotation, the prior
/// next-key slot its signature exposed, checked against the digest committed there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
    pub exposed: Option<u32>,
}

/// An AID's key state after its latest accepted event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyState {
    prefix: Primitive,
    sn: u128,
    said: Primitive,
    ilk: Ilk,
    keys: Vec<Primitive>,
    key_threshold: Threshold,
    next_digests: Vec<Primitive>,
    next_threshold: Threshold,
    witnesses: Vec<Primitive>,
    witness_threshold: u128,
    establishment_only: bool,
    delegator: Option<Primitive>,
    signers: Vec<Slot>,
    seals: Vec<Seal>,
}

/// What an event is checked against besides its own key state: the AID's accepted events so far
/// and, for a delegated AID, its delegator's — both in order, the caller's to keep — and who
/// checks it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context<'a> {
    pub accepted: &'a [Event],
    pub delegator: &'a [Event],
    pub validator: Validator,
}

/// Who checks an event. A controller or a witness accepts it without witness receipts and
/// without its delegator's approval, as KERI does, so that a witness can give its receipt and a
/// delegate ask for the approval. keripy's rule for an own event that arrives from another
/// source is the caller's, whose transport knows the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Validator {
    /// Any validator given an event from another node, its own AID's event included.
    #[default]
    Remote,
    /// The node that created the event, for an AID it controls.
    Controller,
    /// A witness of the AID, given the event by its controller to receipt.
    Witness,
}

impl KeyState {
    /// The key state after `message`, which follows `state` — or incepts the AID when `state` is
    /// `None`.
    pub fn accept(
        state: Option<&Self>,
        message: &Message,
        context: Context<'_>,
    ) -> Result<Self, Verdict> {
        let event = message.event();
        if event.ilk() == Ilk::Receipt {
            return Err(refused("a receipt is no event of a KEL"));
        }
        if let Some(verdict) = position(state, message, context.accepted) {
            return Err(verdict);
        }
        match state {
            None => incept(message, context),
            Some(state) => state.advance(message, context),
        }
    }

    pub fn prefix(&self) -> &Primitive {
        &self.prefix
    }

    pub fn sn(&self) -> u128 {
        self.sn
    }

    pub fn said(&self) -> &Primitive {
        &self.said
    }

    /// The type of the latest accepted event.
    pub fn ilk(&self) -> Ilk {
        self.ilk
    }

    pub fn keys(&self) -> &[Primitive] {
        &self.keys
    }

    pub fn key_threshold(&self) -> &Threshold {
        &self.key_threshold
    }

    pub fn next_digests(&self) -> &[Primitive] {
        &self.next_digests
    }

    pub fn next_threshold(&self) -> &Threshold {
        &self.next_threshold
    }

    pub fn witnesses(&self) -> &[Primitive] {
        &self.witnesses
    }

    pub fn witness_threshold(&self) -> u128 {
        self.witness_threshold
    }

    pub fn establishment_only(&self) -> bool {
        self.establishment_only
    }

    pub fn delegator(&self) -> Option<&Primitive> {
        self.delegator.as_ref()
    }

    /// The keys that signed the latest accepted event, one slot per index.
    pub fn signers(&self) -> &[Slot] {
        &self.signers
    }

    /// The seals the latest accepted event anchors.
    pub fn seals(&self) -> &[Seal] {
        &self.seals
    }

    fn advance(&self, message: &Message, context: Context<'_>) -> Result<Self, Verdict> {
        let event = message.event();
        // keripy's `transferable`: a non-transferable prefix commits to no next key either.
        if self.next_digests.is_empty() {
            return Err(refused("an event of an AID that commits to no next key"));
        }
        if event.prefix() != &self.prefix {
            return Err(refused("an event of another AID"));
        }
        if event.prior() != Some(&self.said) {
            return Err(refused("a prior digest other than the latest event's SAID"));
        }
        match event.ilk() {
            Ilk::Interaction => {
                if self.establishment_only {
                    return Err(refused("an interaction event in an establishment-only KEL"));
                }
                let verified = signed(message, &self.keys, &self.key_threshold, &self.witnesses)?;
                if context.validator == Validator::Remote {
                    receipted(message, &self.witnesses, self.witness_threshold)?;
                }
                Ok(Self {
                    sn: event.sn(),
                    said: event.said().clone(),
                    ilk: event.ilk(),
                    signers: slots(&verified, |_| None),
                    seals: event.seals().to_vec(),
                    ..self.clone()
                })
            }
            Ilk::Rotation | Ilk::DelegatedRotation => {
                // keripy's rule: a delegated AID rotates by `drt` alone; a `drt` of an AID nothing
                // delegates is a rotation, approved by nobody.
                if self.delegator.is_some() && !event.ilk().is_delegated() {
                    return Err(refused("a plain rotation of a delegated AID"));
                }
                if !event.traits().is_empty() {
                    return Err(Refusal::Unsupported("a trait in a rotation".to_owned()).into());
                }
                let key_threshold = established(event.key_threshold(), event.keys())?;
                let next_threshold = established(event.next_threshold(), event.next_digests())?;
                let witnesses = rotated(&self.witnesses, event.cuts(), event.adds())?;
                let witness_threshold = event.witness_threshold().unwrap_or(0);
                let verified = signed(message, event.keys(), &key_threshold, &witnesses)?;
                let exposed =
                    |v: &Verified| v.ondex.and_then(|o| self.exposes(event.keys(), v.index, o));
                let slots_exposed: Vec<usize> = verified
                    .iter()
                    .filter_map(exposed)
                    .filter_map(|o| usize::try_from(o).ok())
                    .collect();
                if !self.next_threshold.met_by(&slots_exposed) {
                    return Err(Verdict::AwaitingSignatures);
                }
                bounded(witness_threshold, &witnesses)?;
                if context.validator == Validator::Remote {
                    receipted(message, &witnesses, witness_threshold)?;
                }
                if let Some(delegator) = self
                    .delegator
                    .as_ref()
                    .filter(|_| context.validator == Validator::Remote)
                {
                    approved(message, delegator, context.delegator)?;
                }
                Ok(Self {
                    sn: event.sn(),
                    said: event.said().clone(),
                    ilk: event.ilk(),
                    keys: event.keys().to_vec(),
                    key_threshold,
                    next_digests: event.next_digests().to_vec(),
                    next_threshold,
                    witnesses,
                    witness_threshold,
                    signers: slots(&verified, exposed),
                    seals: event.seals().to_vec(),
                    ..self.clone()
                })
            }
            _ => Err(refused("an inception of an AID already incepted")),
        }
    }

    /// The prior next-key slot `ondex` when the key at `index` is the one committed there.
    fn exposes(&self, keys: &[Primitive], index: u32, ondex: u32) -> Option<u32> {
        let key = keys.get(usize::try_from(index).ok()?)?;
        let committed = self.next_digests.get(usize::try_from(ondex).ok()?)?;
        (committed.code() == "E"
            && committed.raw() == blake3::hash(key.to_text().as_bytes()).as_bytes())
        .then_some(ondex)
    }
}

/// The verdict on an event at a position already taken, or ahead of the next one.
fn position(state: Option<&KeyState>, message: &Message, accepted: &[Event]) -> Option<Verdict> {
    let event = message.event();
    let next = state.map_or(0, |s| s.sn + 1);
    if event.sn() > next {
        return Some(Verdict::AwaitingPrior { sn: next });
    }
    if event.sn() == next {
        return None;
    }
    let taken = usize::try_from(event.sn())
        .ok()
        .and_then(|sn| accepted.get(sn));
    match taken {
        Some(taken) if taken.said() == event.said() => Some(Verdict::Duplicate),
        Some(taken) => Some(Verdict::LikelyDuplicitous {
            accepted: taken.said().clone(),
            rival: Box::new(message.clone()),
        }),
        None => Some(refused(
            "a position taken by an event the caller did not give",
        )),
    }
}

fn incept(message: &Message, context: Context<'_>) -> Result<KeyState, Verdict> {
    let event = message.event();
    if !event.ilk().is_inception() {
        return Err(Verdict::AwaitingPrior { sn: 0 });
    }
    if event.sn() != 0 {
        return Err(refused("an inception past sequence number 0"));
    }
    let establishment_only = event
        .traits()
        .iter()
        .try_fold(false, |_, t| match t.as_str() {
            "EO" => Ok(true),
            other => Err(Refusal::Unsupported(alloc::format!("the trait {other}"))),
        })?;
    let key_threshold = established(event.key_threshold(), event.keys())?;
    let next_threshold = established(event.next_threshold(), event.next_digests())?;
    let witnesses = event.witnesses().to_vec();
    if witnesses
        .iter()
        .enumerate()
        .any(|(i, w)| witnesses.iter().skip(i + 1).any(|v| v == w))
    {
        return Err(refused("a witness listed twice"));
    }
    let witness_threshold = event.witness_threshold().unwrap_or(0);
    let verified = signed(message, event.keys(), &key_threshold, &witnesses)?;
    bounded(witness_threshold, &witnesses)?;
    if context.validator == Validator::Remote {
        receipted(message, &witnesses, witness_threshold)?;
    }
    let delegator = event.delegator().cloned();
    if let Some(delegator) = delegator
        .as_ref()
        .filter(|_| context.validator == Validator::Remote)
    {
        approved(message, delegator, context.delegator)?;
    }
    Ok(KeyState {
        prefix: event.prefix().clone(),
        sn: 0,
        said: event.said().clone(),
        ilk: event.ilk(),
        keys: event.keys().to_vec(),
        key_threshold,
        next_digests: event.next_digests().to_vec(),
        next_threshold,
        witnesses,
        witness_threshold,
        establishment_only,
        delegator,
        signers: slots(&verified, |_| None),
        seals: event.seals().to_vec(),
    })
}

/// A threshold an establishment event sets, weighing no more keys than it lists.
fn established(threshold: Option<&Threshold>, items: &[Primitive]) -> Result<Threshold, Verdict> {
    let threshold =
        threshold.ok_or_else(|| refused("an establishment event without a threshold"))?;
    let weighs = match threshold {
        Threshold::Count(n) => *n,
        Threshold::Weighted(_) => threshold
            .size()
            .and_then(|s| u128::try_from(s).ok())
            .unwrap_or(u128::MAX),
    };
    if u128::try_from(items.len()).is_ok_and(|n| weighs > n) {
        return Err(refused("a threshold over more keys than listed"));
    }
    Ok(threshold.clone())
}

/// A witness threshold is 0 for no witness, and from 1 to the number of witnesses otherwise.
fn bounded(threshold: u128, witnesses: &[Primitive]) -> Result<(), Verdict> {
    let count = u128::try_from(witnesses.len()).unwrap_or(u128::MAX);
    let fits = if witnesses.is_empty() {
        threshold == 0
    } else {
        (1..=count).contains(&threshold)
    };
    if fits {
        Ok(())
    } else {
        Err(refused("a witness threshold outside its list"))
    }
}

/// The witness list after a rotation: the prior list without its cuts, then its adds.
fn rotated(
    prior: &[Primitive],
    cuts: &[Primitive],
    adds: &[Primitive],
) -> Result<Vec<Primitive>, Verdict> {
    let distinct = |list: &[Primitive]| {
        list.iter()
            .enumerate()
            .all(|(i, w)| !list.iter().skip(i + 1).any(|v| v == w))
    };
    if !distinct(cuts) || !distinct(adds) {
        return Err(refused("a witness cut or added twice"));
    }
    if !cuts.iter().all(|c| prior.contains(c)) {
        return Err(refused("a cut of a witness not listed"));
    }
    if adds.iter().any(|a| cuts.contains(a) || prior.contains(a)) {
        return Err(refused("an add of a witness listed or cut"));
    }
    Ok(prior
        .iter()
        .filter(|w| !cuts.contains(w))
        .chain(adds)
        .cloned()
        .collect())
}

/// A controller's signature that verifies under the key at its index, with the prior next-key
/// index its code claims.
struct Verified {
    index: u32,
    ondex: Option<u32>,
}

/// The signatures that verify under `keys`, once they meet `threshold`; a witness whose prefix is
/// no key refuses the event once a signature verifies, where keripy refuses it.
fn signed(
    message: &Message,
    keys: &[Primitive],
    threshold: &Threshold,
    witnesses: &[Primitive],
) -> Result<Vec<Verified>, Verdict> {
    let text = message.event().text().as_bytes();
    let verified: Vec<Verified> = message
        .signatures()
        .iter()
        .filter(|s| key_at(keys, s).is_some_and(|key| signature::indexed_verifies(key, text, s)))
        .map(|s| Verified {
            index: s.index(),
            ondex: s.ondex(),
        })
        .collect();
    if verified.is_empty() {
        return Err(refused("no signature that verifies"));
    }
    if !witnesses
        .iter()
        .all(|w| matches!(w.code(), "D" | "B" | "1AAJ" | "1AAI"))
    {
        return Err(refused("a witness whose prefix is no key"));
    }
    let indices: Vec<usize> = verified
        .iter()
        .filter_map(|s| usize::try_from(s.index).ok())
        .collect();
    if !threshold.met_by(&indices) {
        return Err(Verdict::AwaitingSignatures);
    }
    Ok(verified)
}

/// One slot per signing index, in the order the signatures came: a copy of a signature, or the
/// same signature under another code, adds no slot.
fn slots(verified: &[Verified], exposed: impl Fn(&Verified) -> Option<u32>) -> Vec<Slot> {
    verified.iter().fold(Vec::new(), |mut slots: Vec<Slot>, v| {
        match slots.iter_mut().find(|s| s.index == v.index) {
            Some(slot) => slot.exposed = slot.exposed.or_else(|| exposed(v)),
            None => slots.push(Slot {
                index: v.index,
                exposed: exposed(v),
            }),
        }
        slots
    })
}

fn key_at<'k>(keys: &'k [Primitive], signature: &IndexedSignature) -> Option<&'k Primitive> {
    keys.get(usize::try_from(signature.index()).ok()?)
}

/// Enough of the listed witnesses receipted the event: by an indexed signature into the list, or
/// by a receipt couple naming one of them.
fn receipted(message: &Message, witnesses: &[Primitive], threshold: u128) -> Result<(), Verdict> {
    let text = message.event().text().as_bytes();
    let mut receiptors: Vec<&Primitive> = message
        .witness_signatures()
        .iter()
        .filter_map(|s| key_at(witnesses, s).filter(|w| signature::indexed_verifies(w, text, s)))
        .chain(
            message
                .receipts()
                .iter()
                .filter(|(w, s)| witnesses.contains(w) && signature::plain_verifies(w, text, s))
                .map(|(w, _)| w),
        )
        .collect();
    receiptors.sort_by(|a, b| a.to_text().cmp(b.to_text()));
    receiptors.dedup();
    if u128::try_from(receiptors.len()).is_ok_and(|n| n >= threshold) {
        Ok(())
    } else {
        Err(Verdict::AwaitingReceipts)
    }
}

/// The delegator's accepted events anchor a key event seal of the delegated event — in the event
/// the seal source couple names, when the message carries one.
fn approved(message: &Message, delegator: &Primitive, events: &[Event]) -> Result<(), Verdict> {
    let event = message.event();
    let waiting = || Verdict::AwaitingDelegator {
        delegator: delegator.clone(),
    };
    let seal = Seal::Event {
        prefix: event.prefix().clone(),
        sn: event.sn(),
        said: event.said().clone(),
    };
    let anchors = |e: &&Event| e.prefix() == delegator && e.seals().contains(&seal);
    let found = match message.source() {
        Some(source) => events
            .iter()
            .find(|e| e.sn() == source.sn && e.said() == &source.said)
            .filter(|e| anchors(e))
            .or_else(|| events.iter().find(anchors)),
        None => events.iter().find(anchors),
    };
    found.map(|_| ()).ok_or_else(waiting)
}

fn refused(why: &str) -> Verdict {
    Verdict::Refused(Refusal::Invalid(String::from(why)))
}

/// The key state a KEL's messages fold to, each accepted in turn as by [`Validator::Remote`], with
/// the accepted events; the first message that is not accepted stops the fold with its verdict.
/// A controller or a witness folds its KEL through [`KeyState::accept`], each event checked as
/// it was.
pub fn fold<'m>(
    messages: impl IntoIterator<Item = &'m Message>,
    delegator: &[Event],
) -> Result<(KeyState, Vec<Event>), Verdict> {
    let (state, events) = messages.into_iter().try_fold(
        (None::<KeyState>, Vec::new()),
        |(state, mut events), message| {
            let context = Context {
                accepted: &events,
                delegator,
                validator: Validator::Remote,
            };
            let next = KeyState::accept(state.as_ref(), message, context)?;
            events.push(message.event().clone());
            Ok::<_, Verdict>((Some(next), events))
        },
    )?;
    state
        .map(|s| (s, events))
        .ok_or_else(|| Verdict::Refused(Refusal::Invalid("an empty KEL".to_owned())))
}
