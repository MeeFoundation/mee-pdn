//! CESR 2.00 count codes: a group's code and its length in quadlets, small (`-K` and 2 digits)
//! or big (`--K` and 5 digits), and the genus-version code. Spec: `kerita-cesr/primitives`.

use alloc::borrow::ToOwned;
use alloc::string::String;

use crate::primitive::Refusal;
use crate::subset;

/// A group header: its code in the small form (`-K`), and how many quadlets the group holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub code: &'static str,
    pub quadlets: usize,
}

/// The largest length a small count code carries.
const SMALL: usize = 64 * 64 - 1;

/// A CESR version a genus-version code names: `-_AAACAA` names 2.00.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenusVersion {
    pub major: usize,
    pub minor: usize,
}

/// The version the crate reads.
pub const CESR_2_00: GenusVersion = GenusVersion { major: 2, minor: 0 };

/// The genus-version code `bytes` start with, with how many characters it takes, or none when
/// they start with another code.
pub fn genus_version(bytes: &[u8]) -> Result<Option<(GenusVersion, usize)>, Refusal> {
    let Some(rest) = bytes.strip_prefix(subset::GENUS_VERSION.as_bytes()) else {
        return Ok(None);
    };
    let digits = rest
        .get(..3)
        .and_then(|d| core::str::from_utf8(d).ok())
        .ok_or_else(|| Refusal::Malformed("a genus-version code cut short".to_owned()))?;
    let version = digits
        .get(..1)
        .and_then(decode)
        .zip(digits.get(1..).and_then(decode))
        .map(|(major, minor)| GenusVersion { major, minor })
        .ok_or_else(|| Refusal::Malformed(alloc::format!("a genus version {digits}")))?;
    Ok(Some((version, subset::GENUS_VERSION.len() + 3)))
}

/// The group header `bytes` start with, read from no more than its own characters, so that the
/// bytes past it are left unread until their size is checked.
pub fn read_bytes(bytes: &[u8]) -> Result<(Group, usize), Refusal> {
    let ascii = bytes.iter().take(8).take_while(|b| b.is_ascii()).count();
    read(core::str::from_utf8(bytes.get(..ascii).unwrap_or_default()).unwrap_or_default())
}

/// The group header `text` starts with, and how many characters it takes.
pub fn read(text: &str) -> Result<(Group, usize), Refusal> {
    let (code, digits) = if text.starts_with("--") {
        (text.get(1..3), text.get(3..8))
    } else if text.starts_with('-') {
        (text.get(..2), text.get(2..4))
    } else {
        return Err(Refusal::Malformed(alloc::format!(
            "{}: no group",
            head(text)
        )));
    };
    let (Some(code), Some(digits)) = (code, digits) else {
        return Err(Refusal::Malformed(alloc::format!(
            "{}: group header cut short",
            head(text)
        )));
    };
    let Some(&known) = subset::COUNTERS.iter().find(|&&c| c == code) else {
        return Err(Refusal::Unsupported(code.to_owned()));
    };
    let quadlets = decode(digits)
        .ok_or_else(|| Refusal::Malformed(alloc::format!("{}: count", head(text))))?;
    let length = if text.starts_with("--") { 8 } else { 4 };
    Ok((
        Group {
            code: known,
            quadlets,
        },
        length,
    ))
}

/// The header of a group of `quadlets` quadlets with the code given in the small form.
pub fn write(code: &str, quadlets: usize) -> String {
    if quadlets <= SMALL {
        alloc::format!("{code}{}", encode(quadlets, 2))
    } else {
        alloc::format!("-{code}{}", encode(quadlets, 5))
    }
}

/// The Base64 digits of `text` as a number, big-endian.
pub fn decode(text: &str) -> Option<usize> {
    text.bytes()
        .try_fold(0usize, |n, b| Some(n.checked_mul(64)? + digit(b)?))
}

/// `n` in `width` Base64 digits, big-endian.
pub fn encode(n: usize, width: usize) -> String {
    (0..width)
        .rev()
        .map(|place| {
            let value = n
                .checked_shr(u32::try_from(6 * place).unwrap_or(u32::MAX))
                .unwrap_or(0);
            char::from(ALPHABET.get(value % 64).copied().unwrap_or(b'A'))
        })
        .collect()
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn digit(b: u8) -> Option<usize> {
    ALPHABET.iter().position(|&a| a == b)
}

fn head(text: &str) -> &str {
    text.get(..8).unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_and_big_headers_read_and_write() {
        assert_eq!(
            read("-KAW"),
            Ok((
                Group {
                    code: "-K",
                    quadlets: 22
                },
                4
            ))
        );
        assert_eq!(
            read("-J__"),
            Ok((
                Group {
                    code: "-J",
                    quadlets: 4095
                },
                4
            ))
        );
        assert_eq!(
            read("--CAABAA"),
            Ok((
                Group {
                    code: "-C",
                    quadlets: 4096
                },
                8
            ))
        );
        assert_eq!(write("-K", 22), "-KAW");
        assert_eq!(write("-C", 4096), "--CAABAA");
    }

    #[test]
    fn a_group_outside_the_subset_is_unsupported() {
        assert_eq!(read("-AAB"), Err(Refusal::Unsupported("-A".to_owned())));
    }

    #[test]
    fn a_genus_version_code_names_its_version() {
        assert_eq!(genus_version(b"-_AAACAA-FAu"), Ok(Some((CESR_2_00, 8))));
        assert_eq!(
            genus_version(b"-_AAABAA"),
            Ok(Some((GenusVersion { major: 1, minor: 0 }, 8)))
        );
        assert_eq!(genus_version(b"-FAu"), Ok(None));
        assert!(matches!(
            genus_version(b"-_AAAC"),
            Err(Refusal::Malformed(_))
        ));
    }

    /// The crate's answer to a lone count code, in keripy's terms: its code with a big form
    /// written as such, and its count; a genus-version code's count is its 3 digits as one number.
    fn answer(text: &str) -> Result<(String, usize), Refusal> {
        if let Some((version, length)) = genus_version(text.as_bytes())? {
            assert_eq!(length, text.len(), "{text}: what the code took");
            return Ok((
                subset::GENUS_VERSION.to_owned(),
                version.major * 4096 + version.minor,
            ));
        }
        let (group, length) = read(text)?;
        assert_eq!(length, text.len(), "{text}: what the header took");
        let code = if text.starts_with("--") {
            alloc::format!("-{}", group.code)
        } else {
            group.code.to_owned()
        };
        Ok((code, group.quadlets))
    }

    /// Every count code of the corpus reads to the code and count keripy read it to when it is
    /// in the subset, and is refused as unsupported when it is not.
    #[test]
    fn counters_read_as_keripy_reads_them() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut counts = [0usize; 3];
        for file in ["keripy/counter.jsonl", "cesr-rs/counter.jsonl"] {
            let text = std::fs::read_to_string(corpus.join(file)).unwrap();
            for line in text.lines() {
                let vector: serde_json::Value = serde_json::from_str(line).unwrap();
                let id = vector["id"].as_str().unwrap();
                let input = vector["input"]["qb64"].as_str().unwrap();
                let answer = answer(input);
                match vector["expect"].as_str().unwrap() {
                    "same" => {
                        let keripy = &vector["keripy"];
                        let theirs = (
                            keripy["code"].as_str().unwrap().to_owned(),
                            usize::try_from(keripy["count"].as_u64().unwrap()).unwrap(),
                        );
                        assert_eq!(answer, Ok(theirs), "{file} {id}: {input}");
                        counts[0] += 1;
                    }
                    "unsupported" => {
                        assert!(
                            matches!(answer, Err(Refusal::Unsupported(_))),
                            "{file} {id}: {input} read as {answer:?}"
                        );
                        counts[1] += 1;
                    }
                    _ => {
                        assert!(answer.is_err(), "{file} {id}: {input} read as {answer:?}");
                        counts[2] += 1;
                    }
                }
            }
        }
        assert!(
            counts[0] > 0 && counts[1] > 0,
            "same, unsupported, refused: {counts:?}"
        );
    }
}
