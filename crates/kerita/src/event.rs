//! A key event in the KERI 2.00 native CESR body, text domain: read, checked against its SAID,
//! and written back. Spec: `kerita/encoding`.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use crate::primitive::{Primitive, Refusal};
use crate::threshold::Threshold;
use kerita_cesr::count;
use kerita_cesr::number;

/// How far a reader goes before refusing, in quadlets: a whole body, any one list in it, and a
/// message's attachments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub body: usize,
    pub list: usize,
    pub attachments: usize,
}

/// The event types of the subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ilk {
    Inception,
    Rotation,
    Interaction,
    DelegatedInception,
    DelegatedRotation,
    Receipt,
}

impl Ilk {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Inception => "icp",
            Self::Rotation => "rot",
            Self::Interaction => "ixn",
            Self::DelegatedInception => "dip",
            Self::DelegatedRotation => "drt",
            Self::Receipt => "rct",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        [
            Self::Inception,
            Self::Rotation,
            Self::Interaction,
            Self::DelegatedInception,
            Self::DelegatedRotation,
            Self::Receipt,
        ]
        .into_iter()
        .find(|ilk| ilk.tag() == tag)
    }

    pub fn is_inception(self) -> bool {
        matches!(self, Self::Inception | Self::DelegatedInception)
    }

    pub fn is_rotation(self) -> bool {
        matches!(self, Self::Rotation | Self::DelegatedRotation)
    }

    pub fn is_establishment(self) -> bool {
        self.is_inception() || self.is_rotation()
    }

    pub fn is_delegated(self) -> bool {
        matches!(self, Self::DelegatedInception | Self::DelegatedRotation)
    }
}

/// A seal an event anchors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seal {
    /// The digest of some data.
    Digest(Primitive),
    /// An event of another KEL: its AID, sequence number and SAID.
    Event {
        prefix: Primitive,
        sn: u128,
        said: Primitive,
    },
}

/// A key event: its fields, and the exact text its signatures cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub(crate) ilk: Ilk,
    pub(crate) said: Primitive,
    pub(crate) prefix: Primitive,
    pub(crate) sn: u128,
    pub(crate) prior: Option<Primitive>,
    pub(crate) key_threshold: Option<Threshold>,
    pub(crate) keys: Vec<Primitive>,
    pub(crate) next_threshold: Option<Threshold>,
    pub(crate) next_digests: Vec<Primitive>,
    pub(crate) witness_threshold: Option<u128>,
    pub(crate) witnesses: Vec<Primitive>,
    pub(crate) cuts: Vec<Primitive>,
    pub(crate) adds: Vec<Primitive>,
    pub(crate) traits: Vec<String>,
    pub(crate) seals: Vec<Seal>,
    pub(crate) delegator: Option<Primitive>,
    pub(crate) text: String,
}

/// The fields of an event before its SAID is computed; [`Event::build`] turns them into one.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Draft {
    pub prefix: Option<Primitive>,
    pub sn: u128,
    pub prior: Option<Primitive>,
    pub key_threshold: Option<Threshold>,
    pub keys: Vec<Primitive>,
    pub next_threshold: Option<Threshold>,
    pub next_digests: Vec<Primitive>,
    pub witness_threshold: u128,
    pub witnesses: Vec<Primitive>,
    pub cuts: Vec<Primitive>,
    pub adds: Vec<Primitive>,
    pub traits: Vec<String>,
    pub seals: Vec<Seal>,
    pub delegator: Option<Primitive>,
}

const DUMMY: char = '#';
const SAID_LENGTH: usize = 44;
const VERSION: &str = "KERICAACAA";

impl Event {
    /// The event `bytes` start with, and how many bytes it takes.
    pub fn read(bytes: &[u8], limits: &Limits) -> Result<(Self, usize), Refusal> {
        match bytes.first() {
            None => return Err(Refusal::Malformed("no event".to_owned())),
            Some(b'{') => return Err(Refusal::Unsupported("a JSON body".to_owned())),
            Some(&b) if !b.is_ascii_alphanumeric() && b != b'-' && b != b'_' => {
                return Err(Refusal::Unsupported(
                    "a binary body or the binary domain".to_owned(),
                ));
            }
            Some(_) => {}
        }
        let (group, header) = count::read_bytes(bytes)?;
        if group.code != "-F" {
            return Err(Refusal::Unsupported(alloc::format!(
                "a body under {}",
                group.code
            )));
        }
        if group.quadlets > limits.body {
            return Err(Refusal::OverLimit(alloc::format!(
                "a body of {} quadlets",
                group.quadlets
            )));
        }
        let end = header + group.quadlets * 4;
        let text = bytes
            .get(..end)
            .ok_or_else(|| Refusal::Malformed("a body cut short".to_owned()))?;
        let text = core::str::from_utf8(text)
            .map_err(|_| Refusal::Unsupported("the binary domain".to_owned()))?;
        let mut cursor = Cursor {
            text: text.get(header..).unwrap_or_default(),
            at: 0,
            limits,
        };
        let event = cursor.event(text, header)?;
        Ok((event, end))
    }

    /// An event from its fields, its SAID computed — and its prefix too, when the draft names
    /// none and the event is an inception — once its text reads back as itself: a draft field
    /// the event type does not carry is refused. A receipt is not built from a draft, which does
    /// not name the event it receipts.
    pub fn build(ilk: Ilk, draft: Draft) -> Result<Self, Refusal> {
        if ilk == Ilk::Receipt {
            return Err(Refusal::Invalid(
                "a receipt from a draft, which names no event it receipts".to_owned(),
            ));
        }
        let dummy = Primitive::new("E", &[0; 32])?;
        let self_addressing = ilk.is_inception() && draft.prefix.is_none();
        let mut event = Self {
            ilk,
            said: dummy.clone(),
            prefix: draft.prefix.unwrap_or_else(|| dummy.clone()),
            sn: draft.sn,
            prior: draft.prior,
            key_threshold: draft.key_threshold,
            keys: draft.keys,
            next_threshold: draft.next_threshold,
            next_digests: draft.next_digests,
            witness_threshold: ilk.is_establishment().then_some(draft.witness_threshold),
            witnesses: draft.witnesses,
            cuts: draft.cuts,
            adds: draft.adds,
            traits: draft.traits,
            seals: draft.seals,
            delegator: draft.delegator,
            text: String::new(),
        };
        let (text, said_at, prefix_at) = event.write()?;
        let said = Primitive::new(
            "E",
            blake3::hash(dummied(&text, said_at, prefix_at.filter(|_| self_addressing)).as_bytes())
                .as_bytes(),
        )?;
        event.said = said.clone();
        if self_addressing {
            event.prefix = said;
        }
        event.text = event.write()?.0;
        let unlimited = Limits {
            body: usize::MAX,
            list: usize::MAX,
            attachments: 0,
        };
        match Event::read(event.text.as_bytes(), &unlimited) {
            Ok((read, used)) if used == event.text.len() && read == event => Ok(read),
            Ok(_) => Err(Refusal::Invalid(
                "a draft field its event type does not carry".to_owned(),
            )),
            Err(refusal) => Err(Refusal::Invalid(alloc::format!(
                "a draft its event type cannot carry: {refusal}"
            ))),
        }
    }

    pub fn ilk(&self) -> Ilk {
        self.ilk
    }

    pub fn said(&self) -> &Primitive {
        &self.said
    }

    pub fn prefix(&self) -> &Primitive {
        &self.prefix
    }

    pub fn sn(&self) -> u128 {
        self.sn
    }

    pub fn prior(&self) -> Option<&Primitive> {
        self.prior.as_ref()
    }

    pub fn key_threshold(&self) -> Option<&Threshold> {
        self.key_threshold.as_ref()
    }

    pub fn keys(&self) -> &[Primitive] {
        &self.keys
    }

    pub fn next_threshold(&self) -> Option<&Threshold> {
        self.next_threshold.as_ref()
    }

    pub fn next_digests(&self) -> &[Primitive] {
        &self.next_digests
    }

    pub fn witness_threshold(&self) -> Option<u128> {
        self.witness_threshold
    }

    /// An inception's witnesses.
    pub fn witnesses(&self) -> &[Primitive] {
        &self.witnesses
    }

    /// The witnesses a rotation cuts.
    pub fn cuts(&self) -> &[Primitive] {
        &self.cuts
    }

    /// The witnesses a rotation adds.
    pub fn adds(&self) -> &[Primitive] {
        &self.adds
    }

    pub fn traits(&self) -> &[String] {
        &self.traits
    }

    pub fn seals(&self) -> &[Seal] {
        &self.seals
    }

    pub fn delegator(&self) -> Option<&Primitive> {
        self.delegator.as_ref()
    }

    /// The exact text of the event: what its signatures and receipts sign.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The event written from its fields, as keripy writes it.
    pub fn to_text(&self) -> Result<String, Refusal> {
        self.write().map(|(text, _, _)| text)
    }

    /// The text, with where the SAID and the prefix start in it.
    fn write(&self) -> Result<(String, usize, Option<usize>), Refusal> {
        let fields = self.fields()?;
        let body: String = fields.iter().map(|(_, text)| text.as_str()).collect();
        let header = count::write("-F", body.len() / 4);
        let offset_of = |label: &str| {
            fields
                .iter()
                .take_while(|(l, _)| *l != label)
                .map(|(_, text)| text.len())
                .sum::<usize>()
                + header.len()
        };
        Ok((
            alloc::format!("{header}{body}"),
            offset_of("d"),
            Some(offset_of("i")),
        ))
    }

    /// The body's fields in order, each with its label and its text.
    pub(crate) fn fields(&self) -> Result<Vec<(&'static str, String)>, Refusal> {
        let mut fields = alloc::vec![
            ("v", Primitive::tag("0O", VERSION)?.to_text().to_owned()),
            (
                "t",
                Primitive::tag("X", self.ilk.tag())?.to_text().to_owned()
            ),
            ("d", self.said.to_text().to_owned()),
            ("i", self.prefix.to_text().to_owned()),
            ("s", number::primitive(self.sn)?.to_text().to_owned()),
        ];
        if let Some(prior) = &self.prior {
            fields.push(("p", prior.to_text().to_owned()));
        }
        if self.ilk.is_establishment() {
            fields.push((
                "kt",
                threshold(self.key_threshold.as_ref())?.to_text().to_owned(),
            ));
            fields.push(("k", list(&self.keys)));
            fields.push((
                "nt",
                threshold(self.next_threshold.as_ref())?
                    .to_text()
                    .to_owned(),
            ));
            fields.push(("n", list(&self.next_digests)));
            let witness_threshold = number::primitive(self.witness_threshold.unwrap_or(0))?;
            fields.push(("bt", witness_threshold.to_text().to_owned()));
            if self.ilk.is_inception() {
                fields.push(("b", list(&self.witnesses)));
            } else {
                fields.push(("br", list(&self.cuts)));
                fields.push(("ba", list(&self.adds)));
            }
            let traits = self
                .traits
                .iter()
                .map(|t| trait_tag(t))
                .collect::<Result<Vec<_>, Refusal>>()?;
            fields.push(("c", list(&traits)));
        }
        if self.ilk != Ilk::Receipt {
            fields.push(("a", seals(&self.seals)?));
        }
        if let Some(delegator) = &self.delegator {
            fields.push(("di", delegator.to_text().to_owned()));
        }
        Ok(fields)
    }
}

fn threshold(threshold: Option<&Threshold>) -> Result<Primitive, Refusal> {
    threshold
        .ok_or_else(|| Refusal::Malformed("an establishment event without a threshold".to_owned()))?
        .primitive()
}

fn list(items: &[Primitive]) -> String {
    let content: String = items.iter().map(Primitive::to_text).collect();
    alloc::format!("{}{content}", count::write("-J", content.len() / 4))
}

fn trait_tag(name: &str) -> Result<Primitive, Refusal> {
    let code = match name.len() {
        2 => "0K",
        3 => "X",
        4 => "1AAF",
        _ => return Err(Refusal::Unsupported(alloc::format!("the trait {name}"))),
    };
    Primitive::tag(code, name)
}

fn seals(seals: &[Seal]) -> Result<String, Refusal> {
    let mut content = String::new();
    // Consecutive seals of one kind share a group, as keripy groups them.
    for run in seals.chunk_by(|a, b| core::mem::discriminant(a) == core::mem::discriminant(b)) {
        let mut group = String::new();
        let code = match run.first() {
            Some(Seal::Digest(_)) => "-Q",
            _ => "-T",
        };
        for seal in run {
            match seal {
                Seal::Digest(digest) => group.push_str(digest.to_text()),
                Seal::Event { prefix, sn, said } => {
                    group.push_str(prefix.to_text());
                    group.push_str(number::primitive(*sn)?.to_text());
                    group.push_str(said.to_text());
                }
            }
        }
        content.push_str(&count::write(code, group.len() / 4));
        content.push_str(&group);
    }
    Ok(alloc::format!(
        "{}{content}",
        count::write("-J", content.len() / 4)
    ))
}

/// The text with the SAID, and a self-addressing prefix when given, replaced by `#`.
fn dummied(text: &str, said_at: usize, prefix_at: Option<usize>) -> String {
    text.char_indices()
        .map(|(i, c)| {
            let in_said = (said_at..said_at + SAID_LENGTH).contains(&i);
            let in_prefix = prefix_at.is_some_and(|p| (p..p + SAID_LENGTH).contains(&i));
            if in_said || in_prefix { DUMMY } else { c }
        })
        .collect()
}

/// A reader over a body or a group within it.
struct Cursor<'a> {
    text: &'a str,
    at: usize,
    limits: &'a Limits,
}

impl<'a> Cursor<'a> {
    fn done(&self) -> bool {
        self.at >= self.text.len()
    }

    fn rest(&self) -> &'a str {
        self.text.get(self.at..).unwrap_or_default()
    }

    fn primitive(&mut self) -> Result<Primitive, Refusal> {
        if self.done() {
            return Err(Refusal::Malformed("a field missing".to_owned()));
        }
        let (primitive, length) = Primitive::read_prefix(self.rest())?;
        self.at += length;
        Ok(primitive)
    }

    fn number(&mut self) -> Result<u128, Refusal> {
        number::value(&self.primitive()?)
    }

    /// The content of the group of `code` that comes next, its length checked first.
    fn group(&mut self, code: &str) -> Result<Cursor<'a>, Refusal> {
        let (group, header) = count::read(self.rest())?;
        if group.code != code {
            return Err(Refusal::Unsupported(alloc::format!(
                "{} where {code} stands",
                group.code
            )));
        }
        if group.quadlets > self.limits.list {
            return Err(Refusal::OverLimit(alloc::format!(
                "a group of {} quadlets",
                group.quadlets
            )));
        }
        let start = self.at + header;
        let end = start + group.quadlets * 4;
        let text = self
            .text
            .get(start..end)
            .ok_or_else(|| Refusal::Malformed("a group cut short".to_owned()))?;
        self.at = end;
        Ok(Cursor {
            text,
            at: 0,
            limits: self.limits,
        })
    }

    fn list(&mut self) -> Result<Vec<Primitive>, Refusal> {
        let mut group = self.group("-J")?;
        core::iter::from_fn(|| (!group.done()).then(|| group.primitive())).collect()
    }

    fn traits(&mut self) -> Result<Vec<String>, Refusal> {
        self.list()?
            .into_iter()
            .map(|tag| match tag.code() {
                "0J" | "0K" | "X" | "1AAF" | "0L" | "0M" | "Y" | "1AAN" | "0N" | "0O" | "Z" => {
                    Ok(tag.soft().to_owned())
                }
                other => Err(Refusal::Malformed(alloc::format!("a trait under {other}"))),
            })
            .collect()
    }

    fn seals(&mut self) -> Result<Vec<Seal>, Refusal> {
        let mut list = self.group("-J")?;
        let mut seals = Vec::new();
        while !list.done() {
            let (group, _) = count::read(list.rest())?;
            let mut content = list.group(group.code)?;
            while !content.done() {
                seals.push(match group.code {
                    "-Q" => Seal::Digest(content.primitive()?),
                    "-T" => Seal::Event {
                        prefix: content.primitive()?,
                        sn: content.number()?,
                        said: content.primitive()?,
                    },
                    other => {
                        return Err(Refusal::Unsupported(alloc::format!("a seal group {other}")));
                    }
                });
            }
        }
        Ok(seals)
    }

    /// The fields of a body, in the order its event type sets; `whole` is the event's text with
    /// its header, which starts `header` characters before the body.
    fn event(&mut self, whole: &str, header: usize) -> Result<Event, Refusal> {
        let version = self.primitive()?;
        if version.code() != "0O" {
            return Err(Refusal::Unsupported(
                "a version field without a genus version".to_owned(),
            ));
        }
        if version.soft() != VERSION {
            return Err(Refusal::Unsupported(alloc::format!(
                "the version {}",
                version.soft()
            )));
        }
        let ilk_tag = self.primitive()?;
        let ilk = Ilk::from_tag(ilk_tag.soft()).ok_or_else(|| {
            Refusal::Unsupported(alloc::format!("the event type {}", ilk_tag.soft()))
        })?;
        let said_at = header + self.at;
        let said = self.primitive()?;
        let prefix_at = header + self.at;
        let prefix = self.primitive()?;
        let sn = self.number()?;
        let prior = (ilk.is_rotation() || ilk == Ilk::Interaction)
            .then(|| self.primitive())
            .transpose()?;
        let mut event = Event {
            ilk,
            said,
            prefix,
            sn,
            prior,
            key_threshold: None,
            keys: Vec::new(),
            next_threshold: None,
            next_digests: Vec::new(),
            witness_threshold: None,
            witnesses: Vec::new(),
            cuts: Vec::new(),
            adds: Vec::new(),
            traits: Vec::new(),
            seals: Vec::new(),
            delegator: None,
            text: whole.to_owned(),
        };
        if ilk.is_establishment() {
            event.key_threshold = Some(Threshold::read(&self.primitive()?)?);
            event.keys = self.list()?;
            event.next_threshold = Some(Threshold::read(&self.primitive()?)?);
            event.next_digests = self.list()?;
            event.witness_threshold = Some(self.number()?);
            if ilk.is_inception() {
                event.witnesses = self.list()?;
            } else {
                event.cuts = self.list()?;
                event.adds = self.list()?;
            }
            event.traits = self.traits()?;
        }
        if ilk != Ilk::Receipt {
            event.seals = self.seals()?;
        }
        if ilk == Ilk::DelegatedInception {
            event.delegator = Some(self.primitive()?);
        }
        if !self.done() {
            return Err(Refusal::Malformed(
                "fields past the event type's last".to_owned(),
            ));
        }
        check_said(&event, said_at, prefix_at)?;
        // keripy writes the event again and refuses one whose text differs.
        if event.to_text()? != event.text {
            return Err(Refusal::Malformed(
                "an event not as keripy writes it".to_owned(),
            ));
        }
        check_prefix(&event)?;
        Ok(event)
    }
}

/// The prefix codes of the subset, keripy's `PreDex`: basic, then self-addressing.
const PREFIXES: [&str; 5] = ["D", "B", "1AAJ", "1AAI", "E"];

/// The basic prefix codes that commit to no next key, keripy's `NonTransDex`.
const NON_TRANSFERABLE: [&str; 2] = ["B", "1AAI"];

/// The rules keripy checks of a prefix as it reads the event (`SerderKERI._verify`), before any
/// key state.
fn check_prefix(event: &Event) -> Result<(), Refusal> {
    let invalid = |why: &str| Err(Refusal::Invalid(why.to_owned()));
    let code = event.prefix.code();
    if !PREFIXES.contains(&code) {
        return invalid("a prefix under a code no AID takes");
    }
    if event.ilk.is_delegated() && code != "E" {
        return invalid("a delegated AID that is not self-addressing");
    }
    if event.ilk.is_inception() && code != "E" {
        if event.keys != core::slice::from_ref(&event.prefix) {
            return invalid("a basic prefix that is not the event's one key");
        }
        if event.key_threshold != Some(Threshold::Count(1)) {
            return invalid("a basic prefix under a threshold other than 1");
        }
    }
    if event.ilk.is_establishment()
        && NON_TRANSFERABLE.contains(&code)
        && (!event.next_digests.is_empty()
            || !event.witnesses.is_empty()
            || !event.seals.is_empty())
    {
        return invalid("a non-transferable AID that commits to more");
    }
    if event
        .delegator
        .as_ref()
        .is_some_and(|d| !PREFIXES.contains(&d.code()))
    {
        return invalid("a delegator under a code no AID takes");
    }
    Ok(())
}

/// The SAID is the Blake3 digest of the text with the SAID — and an inception's self-addressing
/// prefix — replaced by `#`, and that prefix is the SAID.
fn check_said(event: &Event, said_at: usize, prefix_at: usize) -> Result<(), Refusal> {
    if event.ilk == Ilk::Receipt {
        return Ok(());
    }
    let self_addressing = event.ilk.is_inception() && event.prefix.code() == "E";
    let text = dummied(&event.text, said_at, self_addressing.then_some(prefix_at));
    let digest = blake3::hash(text.as_bytes());
    if event.said.code() != "E" || event.said.raw() != digest.as_bytes() {
        return Err(Refusal::Malformed(
            "the SAID is not the body's digest".to_owned(),
        ));
    }
    if self_addressing && event.prefix != event.said {
        return Err(Refusal::Malformed(
            "a self-addressing prefix that is not the SAID".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trait reads as the tag its primitive holds, under each tag code that pads it: `0J_A`
    /// holds `A`, and `0L_ABCDE` holds `ABCDE`.
    #[test]
    fn a_trait_reads_as_its_tag() {
        let limits = Limits {
            body: 64,
            list: 64,
            attachments: 0,
        };
        for (text, tag) in [
            ("-JAB0KEO", "EO"),
            ("-JAB0J_A", "A"),
            ("-JAC0L_ABCDE", "ABCDE"),
        ] {
            let mut cursor = Cursor {
                text,
                at: 0,
                limits: &limits,
            };
            assert_eq!(cursor.traits(), Ok(alloc::vec![tag.to_owned()]), "{text}");
        }
    }

    /// `event` with `change` made to its fields, its text left as it was.
    fn changed(event: &Event, change: impl FnOnce(&mut Event)) -> Event {
        let mut event = event.clone();
        change(&mut event);
        event
    }

    fn primitive(code: &str, size: usize, byte: u8) -> Primitive {
        Primitive::new(code, &alloc::vec![byte; size]).unwrap()
    }

    /// The key's own inception under its basic prefix, a non-transferable one committing to
    /// nothing, and a self-addressing delegated one.
    fn kept() -> [Event; 3] {
        let (key, digest, lone) = (
            primitive("D", 32, 1),
            primitive("E", 32, 3),
            primitive("B", 32, 5),
        );
        let draft = Draft {
            prefix: Some(key.clone()),
            key_threshold: Some(Threshold::Count(1)),
            keys: alloc::vec![key],
            next_threshold: Some(Threshold::Count(1)),
            next_digests: alloc::vec![digest.clone()],
            ..Draft::default()
        };
        let basic = Event::build(Ilk::Inception, draft).unwrap();
        let non_transferable = changed(&basic, |e| {
            e.prefix = lone.clone();
            e.keys = alloc::vec![lone];
            e.next_threshold = Some(Threshold::Count(0));
            e.next_digests = Vec::new();
        });
        let delegated = changed(&basic, |e| {
            e.ilk = Ilk::DelegatedInception;
            e.prefix = digest.clone();
            e.delegator = Some(digest);
        });
        [basic, non_transferable, delegated]
    }

    /// An event breaking a rule keripy checks of a prefix as it reads the event is invalid, and
    /// an event keeping every rule passes: the key's own inception under its basic prefix, a
    /// non-transferable one committing to nothing, a self-addressing delegated one.
    #[test]
    fn a_prefix_breaking_a_rule_keripy_reads_by_is_invalid() {
        let [basic, lone, delegated] = kept();
        for event in [&basic, &lone, &delegated] {
            assert_eq!(check_prefix(event), Ok(()), "{event:?}");
        }
        let (key, other, digest) = (
            primitive("D", 32, 1),
            primitive("D", 32, 2),
            primitive("E", 32, 3),
        );
        let signature = primitive("0B", 64, 4);
        let one = crate::threshold::Weight::new(1, 1).unwrap();
        let weighted = Threshold::weighted(alloc::vec![alloc::vec![one]]).unwrap();
        let broken = [
            (
                "a signature's code",
                changed(&basic, |e| e.prefix = signature.clone()),
            ),
            (
                "another key",
                changed(&basic, |e| e.keys = alloc::vec![other.clone()]),
            ),
            ("a second key", changed(&basic, |e| e.keys.push(other))),
            (
                "a weighted threshold",
                changed(&basic, |e| e.key_threshold = Some(weighted)),
            ),
            (
                "a delegated rotation",
                changed(&basic, |e| e.ilk = Ilk::DelegatedRotation),
            ),
            (
                "a delegated inception",
                changed(&delegated, |e| e.prefix = key),
            ),
            (
                "a delegator's code",
                changed(&delegated, |e| e.delegator = Some(signature)),
            ),
            (
                "a next key",
                changed(&lone, |e| e.next_digests.push(digest.clone())),
            ),
            (
                "a witness",
                changed(&lone, |e| e.witnesses.push(primitive("B", 32, 6))),
            ),
            (
                "a seal",
                changed(&lone, |e| e.seals.push(Seal::Digest(digest))),
            ),
            (
                "a rotation committing to a next key",
                changed(&lone, |e| {
                    e.ilk = Ilk::Rotation;
                    e.next_digests.push(primitive("E", 32, 7));
                }),
            ),
        ];
        for (case, event) in broken {
            assert!(
                matches!(check_prefix(&event), Err(Refusal::Invalid(_))),
                "{case}"
            );
        }
    }
}
