use crate::cesr::core::matter::builder::MatterBuilder;
use crate::cesr::core::matter::code::{CesrCode as _, DigestCode};
use crate::cesr::core::primitives::Diger;
#[allow(
    unused_imports,
    reason = "alloc prelude items; subset used per cfg/feature combination"
)]
use alloc::string::ToString;

/// Computes a cryptographic digest of `data` using the algorithm specified by `code`.
///
/// Thin free-function alias for [`Diger::digest`], kept for call sites that read
/// as a producer.
///
/// # Errors
///
/// Returns a [`DigestError`](crate::cesr::crypto::error::DigestError) if building the CESR primitive fails.
pub fn digest(
    code: DigestCode,
    data: &[u8],
) -> Result<Diger<'static>, crate::cesr::crypto::error::DigestError> {
    Diger::digest(code, data)
}

impl Diger<'_> {
    /// Computes a cryptographic digest of `data` under `code`, returning the
    /// resulting [`Diger`]. The write-side companion to [`Diger::verify`].
    ///
    /// # Errors
    ///
    /// Returns a [`DigestError`](crate::cesr::crypto::error::DigestError) if building the CESR primitive fails.
    pub fn digest(
        code: DigestCode,
        data: &[u8],
    ) -> Result<Diger<'static>, crate::cesr::crypto::error::DigestError> {
        let raw = match code {
            DigestCode::Blake3_256 => blake3::hash(data).as_bytes().to_vec(),
            other => {
                return Err(crate::cesr::crypto::error::DigestError::Unsupported(
                    other.as_str().to_string(),
                ));
            }
        };
        MatterBuilder::new()
            .with_code(code)
            .with_raw(raw)
            .map_err(|e| crate::cesr::crypto::error::DigestError::BuildFailed(e.to_string()))?
            .build()
            .map_err(|e| crate::cesr::crypto::error::DigestError::BuildFailed(e.to_string()))
    }

    /// Returns `true` if `data` hashes, under this digest's own code, to this digest.
    ///
    /// The read-side companion to [`Diger::digest`]: it recomputes the digest of
    /// `data` with `self`'s algorithm and compares the raw bytes. It fails closed —
    /// if the recomputation cannot be built, the answer is `false` rather than an
    /// error — so `false` unconditionally means "does not commit to `data`".
    #[must_use]
    pub fn verify(&self, data: &[u8]) -> bool {
        Self::digest(*self.code(), data).is_ok_and(|computed| computed.raw() == self.raw())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cesr::core::matter::code::DigestCode;

    #[test]
    fn blake3_256_digest() {
        let d = digest(DigestCode::Blake3_256, b"hello").unwrap();
        assert_eq!(*d.code(), DigestCode::Blake3_256);
        assert_eq!(d.raw().len(), 32);
        assert!(d.verify(b"hello"));
        assert!(!d.verify(b"world"));
    }

    #[test]
    fn other_digests_are_unsupported() {
        assert!(digest(DigestCode::SHA2_256, b"hello").is_err());
    }
}
