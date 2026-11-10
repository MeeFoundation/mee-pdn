//! A number — a sequence number, a count threshold, a witness threshold — as CESR writes it: the
//! smallest of keripy's number codes its value fits, big-endian.

use alloc::borrow::ToOwned;

use crate::primitive::{Primitive, Refusal};

/// Number codes by the bytes they hold, smallest first.
const CODES: &[(&str, usize)] = &[
    ("M", 2),
    ("0H", 4),
    ("R", 5),
    ("N", 8),
    ("S", 11),
    ("T", 14),
    ("0A", 16),
    ("U", 17),
];

/// The value a number primitive holds; a value past 2^128 is outside the subset.
pub fn value(primitive: &Primitive) -> Result<u128, Refusal> {
    if !CODES.iter().any(|&(code, _)| code == primitive.code()) {
        return Err(Refusal::Malformed(alloc::format!(
            "{}: not a number",
            primitive.to_text()
        )));
    }
    primitive.raw().iter().try_fold(0u128, |n, &b| {
        n.checked_mul(256)
            .map(|n| n + u128::from(b))
            .ok_or_else(|| Refusal::Unsupported("a number past 2^128".to_owned()))
    })
}

/// The number primitive keripy writes for `n`.
pub fn primitive(n: u128) -> Result<Primitive, Refusal> {
    let bytes = n.to_be_bytes();
    let significant = bytes.len() - bytes.iter().take_while(|&&b| b == 0).count();
    let &(code, size) = CODES
        .iter()
        .find(|&&(_, size)| size >= significant)
        .ok_or_else(|| Refusal::Unsupported("a number past 2^128".to_owned()))?;
    let mut raw = alloc::vec![0u8; size.saturating_sub(bytes.len())];
    raw.extend_from_slice(
        bytes
            .get(bytes.len().saturating_sub(size)..)
            .unwrap_or(&bytes),
    );
    Primitive::new(code, &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_numbers_take_the_short_code() {
        assert_eq!(primitive(0).unwrap().to_text(), "MAAA");
        assert_eq!(primitive(1).unwrap().to_text(), "MAAB");
        assert_eq!(value(&Primitive::read("MAAC").unwrap()), Ok(2));
    }

    #[test]
    fn a_number_round_trips_at_each_width() {
        for n in [0, 65_535, 65_536, u128::from(u64::MAX), u128::MAX] {
            assert_eq!(value(&primitive(n).unwrap()), Ok(n));
        }
    }
}
