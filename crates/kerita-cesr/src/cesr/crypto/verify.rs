use crate::cesr::core::matter::code::VerKeyCode;
use crate::cesr::core::primitives::{Siger, Verfer};
#[allow(
    unused_imports,
    reason = "alloc prelude items; subset used per cfg/feature combination"
)]
use alloc::{format, string::ToString, vec};

use crate::cesr::crypto::algo::{Algorithm, Ed25519, Secp256r1};
use crate::cesr::crypto::error::{
    CodeMismatchError, IndexedVerifyError, SignatureError, VerificationError,
};
use crate::cesr::crypto::signature::Signature;

/// Verifies `sig` over `data` using the algorithm indicated by `verfer`'s CESR
/// code — the verifier-key-driven entry point for KERI verification, where the
/// verifier holds only public keys.
///
/// Generic over [`Signature`], so the *same* function verifies both non-indexed
/// ([`Cigar`](crate::cesr::core::primitives::Cigar)) and indexed
/// ([`Siger`](crate::cesr::core::primitives::Siger)) signatures — the caller never
/// branches on "indexed or not". It mirrors keripy's
/// `siger.verfer.verify(siger.raw, ser)` while composing into lazy iterator
/// chains over `stream`-parsed signature groups:
/// `sigers.try_for_each(|s| verify(verfer, msg, s))`.
///
/// `Ok(())` means valid. The check is strict: the signature's code must belong
/// to `verfer`'s algorithm, otherwise [`CodeMismatchError`] is returned rather
/// than a silent failure. (For a [`Siger`](crate::cesr::core::primitives::Siger) the
/// index is CESR framing metadata and is not part of the signed payload.)
///
/// # Errors
///
/// Returns [`VerificationError::CodeMismatch`] if the signature's code does not
/// match `verfer`'s algorithm, [`VerificationError::UnsupportedAlgorithm`] for
/// a recognized key code without a verifier (Ed448), or
/// [`VerificationError::Signature`] wrapping [`SignatureError::Invalid`] if
/// the signature does not match — or another [`SignatureError`] if the key or
/// signature bytes are malformed.
pub fn verify<S: Signature>(
    verfer: &Verfer<'_>,
    data: &[u8],
    sig: &S,
) -> Result<(), VerificationError> {
    match verfer.code() {
        VerKeyCode::Ed25519 | VerKeyCode::Ed25519N => verify_as::<Ed25519, S>(verfer, data, sig),
        VerKeyCode::ECDSA256r1 | VerKeyCode::ECDSA256r1N => {
            verify_as::<Secp256r1, S>(verfer, data, sig)
        }
        other => Err(VerificationError::UnsupportedAlgorithm { verkey: *other }),
    }
}

/// Verifies each indexed signature against the key it addresses, lazily.
///
/// Each item is the signature's key-index on success, or the failure that stopped
/// it: the index addressed no key ([`IndexedVerifyError::IndexOutOfRange`]) or the
/// cryptographic check failed ([`IndexedVerifyError::Verification`]).
///
/// The resolve step [`verify`] cannot do alone: an indexed signature means
/// "signature by the key at position `index`", so this maps `siger.index()` onto
/// `keys[index]` and then defers to [`verify`]. Zero-alloc and borrowing — it
/// yields an iterator that holds only borrows of `keys`, `data`, and `sigs`. The
/// index is CESR framing metadata and is not part of the signed payload.
///
/// Compose threshold satisfaction over the yielded indices:
/// `verify_indexed(keys, data, sigs).collect::<Result<Vec<_>, _>>()?` then
/// `threshold.satisfied_by(indices)`.
pub fn verify_indexed<'a>(
    keys: &'a [Verfer<'a>],
    data: &'a [u8],
    sigs: impl IntoIterator<Item = &'a Siger<'a>> + 'a,
) -> impl Iterator<Item = Result<u32, IndexedVerifyError>> + 'a {
    sigs.into_iter().map(move |sig| {
        let index = sig.index();
        let out_of_range = || IndexedVerifyError::IndexOutOfRange {
            index,
            key_count: keys.len(),
        };
        let position = usize::try_from(index).map_err(|_| out_of_range())?;
        let key = keys.get(position).ok_or_else(out_of_range)?;
        verify(key, data, sig)?;
        Ok(index)
    })
}

/// Verifies `sig` as algorithm `A`: strict code-ownership check, then the
/// compile-time-dispatched per-curve crypto via [`Algorithm::verify_bytes`].
fn verify_as<A: Algorithm, S: Signature>(
    verfer: &Verfer<'_>,
    data: &[u8],
    sig: &S,
) -> Result<(), VerificationError> {
    if !sig.belongs_to::<A>() {
        return Err(VerificationError::CodeMismatch(
            CodeMismatchError::IncompatibleCodes {
                verkey: format!("{:?}", verfer.code()),
                signature: sig.code_name(),
            },
        ));
    }
    A::verify_bytes(verfer.raw(), data, sig.raw()).map_err(VerificationError::from)
}

pub(crate) fn verify_ed25519(key: &[u8], data: &[u8], sig: &[u8]) -> Result<(), SignatureError> {
    use ed25519_dalek::{Signature, VerifyingKey};

    let vk_bytes: [u8; 32] = key.try_into().map_err(|_| {
        SignatureError::VerificationFailed(format!(
            "invalid Ed25519 public key length: {}",
            key.len()
        ))
    })?;
    let verifying_key = VerifyingKey::from_bytes(&vk_bytes)
        .map_err(|e| SignatureError::VerificationFailed(e.to_string()))?;
    let sig_bytes: [u8; 64] =
        sig.try_into()
            .map_err(|_| SignatureError::InvalidSignatureLength {
                expected: 64,
                actual: sig.len(),
            })?;
    verifying_key
        .verify_strict(data, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| SignatureError::Invalid)
}

/// Accepts either S value of a signature, as keripy does (`test_high_s_signature` in the corpus).
pub(crate) fn verify_secp256r1(key: &[u8], data: &[u8], sig: &[u8]) -> Result<(), SignatureError> {
    use p256::ecdsa::signature::Verifier as _;
    use p256::ecdsa::{Signature, VerifyingKey};

    let verifying_key = VerifyingKey::from_sec1_bytes(key)
        .map_err(|e| SignatureError::VerificationFailed(e.to_string()))?;
    if sig.len() != 64 {
        return Err(SignatureError::InvalidSignatureLength {
            expected: 64,
            actual: sig.len(),
        });
    }
    let signature = Signature::from_slice(sig)
        .map_err(|e| SignatureError::VerificationFailed(e.to_string()))?;
    let signature = signature.normalize_s();
    verifying_key
        .verify(data, &signature)
        .map_err(|_| SignatureError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];

    #[test]
    fn ed25519_signature_verifies_and_a_changed_byte_does_not() {
        use ed25519_dalek::Signer as _;
        let key = ed25519_dalek::SigningKey::from_bytes(&SEED);
        let sig = key.sign(b"hello");
        let public = key.verifying_key().to_bytes();
        assert!(verify_ed25519(&public, b"hello", &sig.to_bytes()).is_ok());
        assert!(verify_ed25519(&public, b"hellp", &sig.to_bytes()).is_err());
    }

    #[test]
    fn p256_signature_verifies_in_both_s_forms() {
        use p256::ecdsa::signature::Signer as _;
        let key = p256::ecdsa::SigningKey::from_slice(&SEED).unwrap();
        let sig: p256::ecdsa::Signature = key.sign(b"hello");
        let public = key.verifying_key().to_sec1_point(true);
        let low = sig.normalize_s().to_bytes();
        let (r, s) = sig.split_scalars();
        let high = p256::ecdsa::Signature::from_scalars(r, -*s)
            .unwrap()
            .to_bytes();
        assert!(verify_secp256r1(public.as_bytes(), b"hello", &low).is_ok());
        assert!(verify_secp256r1(public.as_bytes(), b"hello", &high).is_ok());
        assert!(verify_secp256r1(public.as_bytes(), b"hellp", &low).is_err());
    }
}
