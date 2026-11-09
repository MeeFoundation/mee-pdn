//! A message: an event and its attachments — the controller's indexed signatures, the
//! witnesses' indexed signatures, non-transferable receipt couples, and the seal source couple of
//! a delegated event — in one attachment group, as keripy writes them. Spec: `kerita/encoding`.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use crate::event::{Event, Limits};
use crate::primitive::{IndexedSignature, Primitive, Refusal};
use crate::signature;
use kerita_cesr::count;
use kerita_cesr::number;

/// Where a delegated event's approval sits: the sequence number and SAID of the delegator's
/// event that anchors it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub sn: u128,
    pub said: Primitive,
}

/// An event with what is attached to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    event: Event,
    signatures: Vec<IndexedSignature>,
    witness_signatures: Vec<IndexedSignature>,
    receipts: Vec<(Primitive, Primitive)>,
    source: Option<Source>,
}

impl Message {
    pub fn new(event: Event) -> Self {
        Self {
            event,
            signatures: Vec::new(),
            witness_signatures: Vec::new(),
            receipts: Vec::new(),
            source: None,
        }
    }

    /// The message with `signature` attached, once it verifies as `key`'s over the event's text.
    pub fn signed(mut self, key: &Primitive, signature: IndexedSignature) -> Result<Self, Refusal> {
        if !signature::indexed_verifies(key, self.event.text().as_bytes(), &signature) {
            return Err(Refusal::Malformed(
                "a signature that does not verify".to_owned(),
            ));
        }
        self.signatures.push(signature);
        Ok(self)
    }

    /// The message with a controller's indexed signature attached unchecked, as a reader
    /// assembling a message from its parts does: the key state verifies it.
    #[must_use]
    pub fn with_signature(mut self, signature: IndexedSignature) -> Self {
        self.signatures.push(signature);
        self
    }

    /// The message with a witness's indexed signature attached, unchecked: which witness list it
    /// indexes into is the key state's to say.
    #[must_use]
    pub fn witnessed(mut self, signature: IndexedSignature) -> Self {
        self.witness_signatures.push(signature);
        self
    }

    /// The message with a non-transferable receipt couple attached: the receiptor's AID and its
    /// signature.
    #[must_use]
    pub fn receipted(mut self, receiptor: Primitive, signature: Primitive) -> Self {
        self.receipts.push((receiptor, signature));
        self
    }

    /// The message with the seal source couple of its delegator's approving event attached.
    #[must_use]
    pub fn sourced(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn event(&self) -> &Event {
        &self.event
    }

    pub fn signatures(&self) -> &[IndexedSignature] {
        &self.signatures
    }

    pub fn witness_signatures(&self) -> &[IndexedSignature] {
        &self.witness_signatures
    }

    pub fn receipts(&self) -> &[(Primitive, Primitive)] {
        &self.receipts
    }

    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }

    /// The message `bytes` start with, and how many bytes it takes — a genus-version code before
    /// it included, which a stream carries to name CESR 2.00 and which keripy reads past; one
    /// naming another version is unsupported, as keripy reads nothing after it.
    pub fn read(bytes: &[u8], limits: &Limits) -> Result<(Self, usize), Refusal> {
        let skipped = match count::genus_version(bytes)? {
            None => 0,
            Some((version, length)) if version == count::CESR_2_00 => length,
            Some((version, _)) => {
                return Err(Refusal::Unsupported(alloc::format!(
                    "the CESR version {}.{:02}",
                    version.major,
                    version.minor
                )));
            }
        };
        let bytes = bytes.get(skipped..).unwrap_or_default();
        let (event, mut at) = Event::read(bytes, limits)?;
        let after = bytes.get(at..).unwrap_or_default();
        let mut message = Self::new(event);
        if !after.starts_with(b"-C") && !after.starts_with(b"--C") {
            return Ok((message, skipped + at));
        }
        let (group, header) = count::read_bytes(after)?;
        if group.quadlets > limits.attachments {
            return Err(Refusal::OverLimit(alloc::format!(
                "attachments of {} quadlets",
                group.quadlets
            )));
        }
        let end = header + group.quadlets * 4;
        let text = after
            .get(..end)
            .ok_or_else(|| Refusal::Malformed("attachments cut short".to_owned()))?;
        let mut rest = core::str::from_utf8(text)
            .map_err(|_| Refusal::Unsupported("the binary domain".to_owned()))?
            .get(header..)
            .unwrap_or_default();
        while !rest.is_empty() {
            let (group, header) = count::read(rest)?;
            let content = rest
                .get(header..header + group.quadlets * 4)
                .ok_or_else(|| Refusal::Malformed("an attachment cut short".to_owned()))?;
            rest = rest.get(header + content.len()..).unwrap_or_default();
            message = message.attach(group.code, content)?;
        }
        at += end;
        Ok((message, skipped + at))
    }

    fn attach(mut self, code: &str, mut content: &str) -> Result<Self, Refusal> {
        while !content.is_empty() {
            match code {
                "-K" | "-L" => {
                    let length = indexed_length(content)?;
                    let signature =
                        IndexedSignature::read(content.get(..length).unwrap_or_default())?;
                    content = content.get(length..).unwrap_or_default();
                    if code == "-K" {
                        self.signatures.push(signature);
                    } else {
                        self.witness_signatures.push(signature);
                    }
                }
                "-M" => {
                    let (receiptor, a) = Primitive::read_prefix(content)?;
                    let (signature, b) =
                        Primitive::read_prefix(content.get(a..).unwrap_or_default())?;
                    content = content.get(a + b..).unwrap_or_default();
                    self.receipts.push((receiptor, signature));
                }
                "-S" => {
                    let (sn, a) = Primitive::read_prefix(content)?;
                    let (said, b) = Primitive::read_prefix(content.get(a..).unwrap_or_default())?;
                    content = content.get(a + b..).unwrap_or_default();
                    self.source = Some(Source {
                        sn: number::value(&sn)?,
                        said,
                    });
                }
                other => {
                    return Err(Refusal::Unsupported(alloc::format!(
                        "an attachment {other}"
                    )));
                }
            }
        }
        Ok(self)
    }

    /// The message as keripy writes it: the event, then one attachment group holding the
    /// controller's signatures, the witnesses', the receipt couples and the seal source couple.
    pub fn to_text(&self) -> Result<String, Refusal> {
        let mut groups = String::new();
        let mut group = |code: &str, content: String| {
            if !content.is_empty() {
                groups.push_str(&count::write(code, content.len() / 4));
                groups.push_str(&content);
            }
        };
        group(
            "-K",
            self.signatures
                .iter()
                .map(IndexedSignature::to_text)
                .collect(),
        );
        group(
            "-L",
            self.witness_signatures
                .iter()
                .map(IndexedSignature::to_text)
                .collect(),
        );
        group(
            "-M",
            self.receipts
                .iter()
                .flat_map(|(r, s)| [r.to_text(), s.to_text()])
                .collect(),
        );
        if let Some(source) = &self.source {
            group(
                "-S",
                alloc::format!(
                    "{}{}",
                    number::primitive(source.sn)?.to_text(),
                    source.said.to_text()
                ),
            );
        }
        let attachments = if groups.is_empty() {
            String::new()
        } else {
            alloc::format!("{}{groups}", count::write("-C", groups.len() / 4))
        };
        Ok(alloc::format!("{}{attachments}", self.event.text()))
    }
}

/// How many characters the indexed signature at the start of `text` takes.
fn indexed_length(text: &str) -> Result<usize, Refusal> {
    // An indexed Ed25519 or P-256 signature: 88 characters small, 92 big.
    let length = if text.starts_with('2') { 92 } else { 88 };
    if text.len() < length {
        return Err(Refusal::Malformed("a signature cut short".to_owned()));
    }
    Ok(length)
}
