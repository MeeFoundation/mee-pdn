#[allow(
    unused_imports,
    reason = "alloc prelude items; subset used per cfg/feature combination"
)]
use alloc::{format, string::String};

use crate::cesr::core::matter::Matter;
use crate::cesr::core::primitives::{Cigar, Siger};
use crate::cesr::crypto::algo::Algorithm;

/// A CESR signature primitive that can be verified against a key pair's
/// algorithm.
///
/// Implemented by both the non-indexed [`Cigar`] and the indexed [`Siger`], so a
/// single generic `verify` covers both forms with compile-time dispatch — the
/// caller never branches on "indexed or not". The algorithm-ownership check
/// ([`belongs_to`](Signature::belongs_to)) is the one runtime seam, because a
/// signature parsed off the wire carries a dynamic code.
pub trait Signature {
    /// The raw signature bytes to verify.
    fn raw(&self) -> &[u8];

    /// Returns `true` if this signature's CESR code belongs to algorithm `A`.
    ///
    /// For a [`Cigar`] this compares against the algorithm's single signature
    /// code; for a [`Siger`] it accepts any of the algorithm's indexed-signature
    /// code variants (small/big, current/both).
    fn belongs_to<A: Algorithm>(&self) -> bool;

    /// The signature's CESR code rendered for diagnostics (error messages).
    fn code_name(&self) -> String;
}

impl Signature for Cigar<'_> {
    fn raw(&self) -> &[u8] {
        Matter::raw(self)
    }

    fn belongs_to<A: Algorithm>(&self) -> bool {
        *self.code() == A::SIGNATURE_CODE
    }

    fn code_name(&self) -> String {
        format!("{:?}", self.code())
    }
}

impl Signature for Siger<'_> {
    fn raw(&self) -> &[u8] {
        Siger::raw(self)
    }

    fn belongs_to<A: Algorithm>(&self) -> bool {
        A::owns_indexed(self.code())
    }

    fn code_name(&self) -> String {
        format!("{:?}", self.code())
    }
}
