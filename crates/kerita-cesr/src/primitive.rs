//! CESR primitives of the subset in the text domain: a key, a digest, a signature, a number, a tag
//! or a Base64 string, and an indexed signature. Spec: `kerita-cesr/primitives`.

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::cesr::core::indexer::IndexerBuilder;
use crate::cesr::core::indexer::code::IndexedSigCode;
use crate::cesr::core::indexer::error::IndexerParseError;
use crate::cesr::core::matter::builder::MatterBuilder;
use crate::cesr::core::matter::code::{CesrCode, MatterCode};
use crate::cesr::core::matter::error::ParsingError;
use crate::subset;

/// Why an input is not read: outside the subset, or not CESR the reference reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("malformed: {0}")]
    Malformed(String),
    /// A group declared longer than the caller's limit, refused before its content is read.
    #[error("over the limit: {0}")]
    OverLimit(String),
    /// Read, but breaking a rule of the protocol it carries, such as KERI's for an event of a KEL.
    #[error("invalid: {0}")]
    Invalid(String),
}

/// A primitive of the subset: its code, its value and, for a tag or a number with one, its soft
/// part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Primitive {
    code: &'static str,
    raw: Vec<u8>,
    soft: String,
    text: String,
}

impl Primitive {
    /// The primitive `text` holds, and nothing after it.
    pub fn read(text: &str) -> Result<Self, Refusal> {
        let matter = MatterBuilder::new()
            .from_qualified_base64(text.as_bytes())
            .map_err(|e| refusal(text, &e.to_string()))?;
        let code = matter.code().as_str();
        if !subset::PRIMITIVES.contains(&code) {
            return Err(Refusal::Unsupported(code.to_owned()));
        }
        let written = matter.to_qb64();
        if written != text {
            return Err(Refusal::Malformed(alloc::format!(
                "{text}: more than one primitive"
            )));
        }
        Ok(Self {
            code,
            raw: matter.raw().to_vec(),
            soft: matter.soft().to_owned(),
            text: written,
        })
    }

    /// A primitive of the subset from its code and value.
    pub fn new(code: &str, raw: &[u8]) -> Result<Self, Refusal> {
        let code = subset_code(code)?;
        let matter = MatterBuilder::new()
            .with_code(code)
            .with_raw(raw)
            .map_err(|e| Refusal::Malformed(e.to_string()))?
            .build()
            .map_err(|e| Refusal::Malformed(e.to_string()))?;
        Self::read(&matter.to_qb64())
    }

    /// A tag of the subset — a version, an event type, a trait — from its code and text.
    pub fn tag(code: &str, soft: &str) -> Result<Self, Refusal> {
        let code = subset_code(code)?;
        let matter = MatterBuilder::new()
            .with_code(code)
            .with_raw(&[][..])
            .map_err(|e| Refusal::Malformed(e.to_string()))?
            .with_soft(soft)
            .map_err(|e| Refusal::Malformed(e.to_string()))?
            .build()
            .map_err(|e| Refusal::Malformed(e.to_string()))?;
        Self::read(&matter.to_qb64())
    }

    /// The primitive `text` starts with, and how many characters it takes.
    pub fn read_prefix(text: &str) -> Result<(Self, usize), Refusal> {
        let matter = MatterBuilder::new()
            .from_qualified_base64(text.as_bytes())
            .map_err(|e| refusal(text, &e.to_string()))?;
        let written = matter.to_qb64();
        if !text.starts_with(written.as_str()) {
            return Err(Refusal::Malformed(alloc::format!("{text}: not canonical")));
        }
        let length = written.len();
        Self::read(&written).map(|p| (p, length))
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn soft(&self) -> &str {
        &self.soft
    }

    pub fn to_text(&self) -> &str {
        &self.text
    }
}

/// An indexed signature of the subset: its code, its index into the current keys, its index into
/// the prior next keys when it has one, and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSignature {
    code: &'static str,
    index: u32,
    ondex: Option<u32>,
    raw: Vec<u8>,
    text: String,
}

impl IndexedSignature {
    /// The indexed signature `raw` makes for a key of `key`'s algorithm, at `index` into the
    /// current keys and at `ondex` into the prior next keys, or in the current keys alone when
    /// `ondex` is `None`; the code is the one keripy picks for those indices.
    pub fn new(
        key: &Primitive,
        index: u32,
        ondex: Option<u32>,
        raw: &[u8],
    ) -> Result<Self, Refusal> {
        let p256 = match key.code() {
            "D" | "B" => false,
            "1AAJ" | "1AAI" => true,
            other => return Err(Refusal::Unsupported(other.to_owned())),
        };
        let small = |both: &str, current: &str| match ondex {
            None => current.to_owned(),
            Some(o) if o == index => both.to_owned(),
            Some(_) => alloc::format!("2{both}"),
        };
        let code = small(if p256 { "E" } else { "A" }, if p256 { "F" } else { "B" });
        let code: IndexedSigCode = IndexedSigCode::from_hard(&code)
            .map_err(|e| Refusal::Malformed(alloc::format!("{e:?}")))?
            .for_index(index.max(ondex.unwrap_or(0)));
        let builder = IndexerBuilder::new().with_code(code);
        let indexed = match ondex {
            None => builder.with_index(index),
            Some(o) => builder.with_indices(index, o),
        }
        .map_err(|e| Refusal::Malformed(alloc::format!("{e:?}")))?;
        let built = indexed
            .with_raw(raw)
            .map_err(|e| Refusal::Malformed(alloc::format!("{e:?}")))?;
        Self::read(&built.to_qb64())
    }

    /// The indexed signature `text` holds, and nothing after it.
    pub fn read(text: &str) -> Result<Self, Refusal> {
        let (indexer, used) = IndexerBuilder::new()
            .from_qb64(text.as_bytes())
            .map_err(|e| {
                match (indexed_code(text), e) {
                    (Some(code), _) if !subset::INDEXED_SIGNATURES.contains(&code) => {
                        Refusal::Unsupported(code.to_owned())
                    }
                    // A code the table here lacks may be one keripy's newer table has.
                    (None, IndexerParseError::UnknownCode(code)) => Refusal::Unsupported(code),
                    (_, e) => Refusal::Malformed(alloc::format!("{e:?}")),
                }
            })?;
        let code = indexer.code().as_str();
        if !subset::INDEXED_SIGNATURES.contains(&code) {
            return Err(Refusal::Unsupported(code.to_owned()));
        }
        if used != text.len() {
            return Err(Refusal::Malformed(alloc::format!(
                "{text}: more than one signature"
            )));
        }
        Ok(Self {
            code,
            index: indexer.index(),
            ondex: indexer.ondex(),
            raw: indexer.raw().to_vec(),
            text: indexer.to_qb64(),
        })
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn ondex(&self) -> Option<u32> {
        self.ondex
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn to_text(&self) -> &str {
        &self.text
    }
}

/// The table's code for a code of the subset.
fn subset_code(code: &str) -> Result<MatterCode, Refusal> {
    if !subset::PRIMITIVES.contains(&code) {
        return Err(Refusal::Unsupported(code.to_owned()));
    }
    code.parse()
        .map_err(|_| Refusal::Unsupported(code.to_owned()))
}

/// What a primitive's text that does not parse is: outside the subset when its code is known
/// and not ours, or one keripy's newer table may have; malformed otherwise.
fn refusal(text: &str, error: &str) -> Refusal {
    match MatterCode::from_base64_stream(text.as_bytes()) {
        Ok(code) if !subset::PRIMITIVES.contains(&code.as_str()) => {
            Refusal::Unsupported(code.as_str().to_owned())
        }
        Err(ParsingError::UnknownMatterCode(code)) => Refusal::Unsupported(code),
        _ => Refusal::Malformed(error.to_owned()),
    }
}

/// The code an indexed signature's text starts with, when the table knows it.
fn indexed_code(text: &str) -> Option<&'static str> {
    [1, 2]
        .into_iter()
        .filter_map(|n| text.get(..n))
        .find_map(|hard| IndexedSigCode::from_hard(hard).ok())
        .map(|code| code.as_str())
}
