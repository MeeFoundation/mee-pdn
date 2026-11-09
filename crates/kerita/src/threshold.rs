//! A signing threshold: a count of keys, or clauses of fractional weights, every clause met.
//! Spec: `kerita/key-state`, "Thresholds are numeric or weighted".

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::primitive::{Primitive, Refusal};
use kerita_cesr::number;

/// A key's weight: a reduced fraction from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weight {
    numerator: u64,
    denominator: u64,
}

impl Weight {
    /// The weight `numerator/denominator`, reduced; one past 1 is malformed.
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, Refusal> {
        if denominator == 0 || numerator > denominator {
            return Err(Refusal::Malformed(alloc::format!(
                "weight {numerator}/{denominator}"
            )));
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    fn parse(text: &str) -> Result<Self, Refusal> {
        let malformed = || Refusal::Malformed(alloc::format!("weight {text:?}"));
        let (numerator, denominator) = match text.split_once('/') {
            Some((n, d)) => (
                n.parse().map_err(|_| malformed())?,
                d.parse().map_err(|_| malformed())?,
            ),
            None => (text.parse().map_err(|_| malformed())?, 1),
        };
        Self::new(numerator, denominator)
    }

    pub fn numerator(self) -> u64 {
        self.numerator
    }

    pub fn denominator(self) -> u64 {
        self.denominator
    }
}

impl fmt::Display for Weight {
    /// As keripy writes a weight: `1/2`, or `1` and `0` whole.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.numerator == 0 || self.numerator == self.denominator {
            write!(f, "{}", self.numerator / self.denominator.max(1))
        } else {
            write!(f, "{}/{}", self.numerator, self.denominator)
        }
    }
}

/// A signing threshold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Threshold {
    /// At least this many keys; 0 is met by no signature, as keripy's.
    Count(u128),
    /// Clauses over consecutive keys, each met when the weights of its signing keys reach 1.
    Weighted(Clauses),
}

/// The clauses of a weighted threshold, each of whose weights sum to at least 1: built by
/// [`Threshold::weighted`] alone, so no clause is one no signers can meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clauses(Vec<Vec<Weight>>);

impl Clauses {
    pub fn as_slice(&self) -> &[Vec<Weight>] {
        &self.0
    }
}

impl Threshold {
    /// The threshold a native body's primitive holds: a number, or a Base64 string of weights.
    pub(crate) fn read(primitive: &Primitive) -> Result<Self, Refusal> {
        if matches!(
            primitive.code(),
            "4A" | "5A" | "6A" | "7AAA" | "8AAA" | "9AAA"
        ) {
            let text = bext(primitive).replace('s', "/");
            if text.contains(['k', 'v']) {
                return Err(Refusal::Unsupported("a nested weight map".to_owned()));
            }
            let clauses = text
                .split('a')
                .map(|clause| clause.split('c').map(Weight::parse).collect())
                .collect::<Result<Vec<Vec<Weight>>, Refusal>>()?;
            Self::weighted(clauses)
        } else {
            number::value(primitive).map(Self::Count)
        }
    }

    /// A weighted threshold whose clauses can each be met, as keripy demands, and whose common
    /// denominator stays within the crate's arithmetic.
    pub fn weighted(clauses: Vec<Vec<Weight>>) -> Result<Self, Refusal> {
        if clauses.is_empty() || clauses.iter().any(Vec::is_empty) {
            return Err(Refusal::Malformed("an empty clause".to_owned()));
        }
        if clauses
            .iter()
            .flatten()
            .try_fold(1u64, |l, w| lcm(l, w.denominator))
            .is_none()
        {
            return Err(Refusal::Unsupported(
                "weights past the crate's precision".to_owned(),
            ));
        }
        let all: Vec<usize> = (0..clauses.iter().map(Vec::len).sum()).collect();
        let mut start = 0usize;
        let reachable = clauses.iter().all(|clause| {
            let met = clause_met(clause, start, &all);
            start += clause.len();
            met
        });
        if !reachable {
            return Err(Refusal::Invalid(
                "a clause whose weights sum below 1".to_owned(),
            ));
        }
        Ok(Self::Weighted(Clauses(clauses)))
    }

    /// The primitive keripy writes for this threshold.
    pub(crate) fn primitive(&self) -> Result<Primitive, Refusal> {
        match self {
            Self::Count(n) => number::primitive(*n),
            Self::Weighted(Clauses(clauses)) => {
                let text = clauses
                    .iter()
                    .map(|c| {
                        c.iter()
                            .map(|w| alloc::format!("{w}").replace('/', "s"))
                            .collect::<Vec<_>>()
                            .join("c")
                    })
                    .collect::<Vec<_>>()
                    .join("a");
                rawify(&text)
            }
        }
    }

    /// How many keys the threshold weighs: a weighted threshold weighs one key per weight.
    pub fn size(&self) -> Option<usize> {
        match self {
            Self::Count(_) => None,
            Self::Weighted(Clauses(clauses)) => Some(clauses.iter().map(Vec::len).sum()),
        }
    }

    /// Whether the keys at the positions `signers` lists meet the threshold.
    pub fn met_by(&self, signers: &[usize]) -> bool {
        match self {
            Self::Count(n) => {
                let mut distinct = signers.to_vec();
                distinct.sort_unstable();
                distinct.dedup();
                *n > 0 && u128::try_from(distinct.len()).is_ok_and(|have| have >= *n)
            }
            Self::Weighted(Clauses(clauses)) => {
                let mut start = 0usize;
                clauses.iter().all(|clause| {
                    let end = start + clause.len();
                    let met = clause_met(clause, start, signers);
                    start = end;
                    met
                })
            }
        }
    }
}

fn clause_met(clause: &[Weight], start: usize, signers: &[usize]) -> bool {
    let Some(denominator) = clause.iter().try_fold(1u64, |l, w| lcm(l, w.denominator)) else {
        return false;
    };
    let mut seen = Vec::new();
    let total: u128 = signers
        .iter()
        .filter_map(|&s| {
            s.checked_sub(start)
                .and_then(|i| clause.get(i))
                .map(|w| (s, w))
        })
        .filter(|&(s, _)| {
            let fresh = !seen.contains(&s);
            seen.push(s);
            fresh
        })
        .map(|(_, w)| u128::from(w.numerator) * u128::from(denominator / w.denominator))
        .sum();
    total >= u128::from(denominator)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

fn lcm(a: u64, b: u64) -> Option<u64> {
    (a / gcd(a, b)).checked_mul(b)
}

/// The text a Base64 string primitive holds, as keripy's `Bexter` reads it.
fn bext(primitive: &Primitive) -> String {
    let lead = match primitive.code() {
        "4A" | "7AAA" => 0,
        "5A" | "8AAA" => 1,
        _ => 2,
    };
    let mut padded = alloc::vec![0u8; lead];
    padded.extend_from_slice(primitive.raw());
    let encoded = URL_SAFE_NO_PAD.encode(&padded);
    let wad = if lead == 0 {
        usize::from(encoded.starts_with('A'))
    } else {
        (lead + 1) % 4
    };
    let text = encoded.get(wad..).unwrap_or_default();
    text.strip_prefix('-').unwrap_or(text).to_owned()
}

/// The Base64 string primitive keripy's `Bexter` writes for `text`.
fn rawify(text: &str) -> Result<Primitive, Refusal> {
    let wad = |t: &str| (4 - t.len() % 4) % 4;
    let escaped = if text.starts_with('-') || (text.starts_with('A') && wad(text) <= 1) {
        alloc::format!("-{text}")
    } else {
        text.to_owned()
    };
    let base = alloc::format!("{}{escaped}", "A".repeat(wad(&escaped)));
    let lead = (3 - escaped.len() % 4) % 3;
    let decoded = URL_SAFE_NO_PAD
        .decode(base.as_bytes())
        .map_err(|e| Refusal::Malformed(alloc::format!("{text}: {e}")))?;
    let raw = decoded.get(lead..).unwrap_or_default();
    let code = ["4A", "5A", "6A"].get(lead).copied().unwrap_or("4A");
    // A variable-size code: its soft part counts the quadlets of lead and value.
    let mut padded = alloc::vec![0u8; lead];
    padded.extend_from_slice(raw);
    let quadlets = padded.len() / 3;
    Primitive::read(&alloc::format!(
        "{code}{}{}",
        kerita_cesr::count::encode(quadlets, 2),
        URL_SAFE_NO_PAD.encode(&padded)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights(text: &[&str]) -> Vec<Weight> {
        text.iter().map(|w| Weight::parse(w).unwrap()).collect()
    }

    /// keripy's `[["1/2","1/2","1/2"],["1","1"]]` over 5 keys, as the key-state spec's example.
    #[test]
    fn every_clause_must_be_met() {
        let t = Threshold::weighted(alloc::vec![
            weights(&["1/2", "1/2", "1/2"]),
            weights(&["1", "1"])
        ])
        .unwrap();
        assert!(t.met_by(&[0, 1, 3]));
        assert!(!t.met_by(&[0, 1]));
        assert!(!t.met_by(&[0, 3, 4]));
        assert!(!t.met_by(&[0, 0, 3]), "a signer counted twice");
    }

    #[test]
    fn a_weighted_threshold_round_trips_through_its_primitive() {
        let t = Threshold::weighted(alloc::vec![weights(&["1/2", "1/2", "1"])]).unwrap();
        assert_eq!(Threshold::read(&t.primitive().unwrap()), Ok(t));
    }

    /// A clause whose weights sum below 1 is refused, as keripy refuses it: no signers meet it.
    #[test]
    fn a_clause_no_signers_meet_is_refused() {
        assert!(matches!(
            Threshold::weighted(alloc::vec![weights(&["1/2"])]),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            Threshold::weighted(alloc::vec![weights(&["1", "1"]), weights(&["1/4", "1/4"])]),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            Threshold::weighted(alloc::vec![]),
            Err(Refusal::Malformed(_))
        ));
        assert!(Threshold::weighted(alloc::vec![weights(&["1/2", "1/2"])]).is_ok());
    }

    /// A count of 0 is met by no signature, as keripy's: an AID committing to it rotates no
    /// more.
    #[test]
    fn a_count_of_zero_is_never_met() {
        assert!(!Threshold::Count(0).met_by(&[]));
        assert!(!Threshold::Count(0).met_by(&[0, 1]));
        assert!(Threshold::Count(1).met_by(&[0]));
    }

    #[test]
    fn whole_weights_are_written_whole() {
        assert_eq!(alloc::format!("{}", Weight::parse("1/1").unwrap()), "1");
        assert_eq!(alloc::format!("{}", Weight::parse("2/4").unwrap()), "1/2");
    }
}
