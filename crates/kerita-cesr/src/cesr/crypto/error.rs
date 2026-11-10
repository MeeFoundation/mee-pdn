#[allow(
    unused_imports,
    reason = "alloc prelude items; subset used per cfg/feature combination"
)]
use alloc::{format, string::String, string::ToString};

use crate::cesr::core::matter::code::VerKeyCode;

/// Errors arising from signing or signature verification operations.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    /// The underlying cryptographic signing operation failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),
    /// The raw signature byte slice has an unexpected length.
    #[error("invalid signature bytes: expected {expected} bytes, got {actual}")]
    InvalidSignatureLength {
        /// Expected byte length.
        expected: usize,
        /// Actual byte length received.
        actual: usize,
    },
    /// The signature is cryptographically invalid: it does not match the key
    /// and data. This is the expected negative outcome of verification (the
    /// former `Ok(false)`), kept out of the success channel so `is_ok()` cannot
    /// mistake a forgery for a valid signature.
    #[error("signature is invalid: does not match key and data")]
    Invalid,
    /// Verification could not be attempted because the verifying key material
    /// is malformed (e.g. wrong length or not a valid curve point).
    #[error("verification failed: {0}")]
    VerificationFailed(String),
    /// The signature's CESR code does not belong to the verifying key pair's
    /// algorithm (e.g. verifying a secp256k1 signature with an Ed25519 key).
    /// Covers both non-indexed (`Cigar`) and indexed (`Siger`) signatures.
    #[error("signature code {actual} does not match key pair algorithm {expected}")]
    CodeMismatch {
        /// The verifying algorithm's name (e.g. `"Ed25519"`).
        expected: String,
        /// The actual signature code found on the signature primitive.
        actual: String,
    },
}

/// Errors arising from digest computation.
#[derive(Debug, thiserror::Error)]
pub enum DigestError {
    /// Building the CESR digest primitive failed.
    #[error("digest build failed: {0}")]
    BuildFailed(String),
    /// The digest code is outside the subset: Blake3-256 alone is computed.
    #[error("unsupported digest code {0}")]
    Unsupported(String),
}

/// Error produced when a verification key code and signature code are incompatible.
#[derive(Debug, thiserror::Error)]
pub enum CodeMismatchError {
    /// The verkey algorithm does not match the signature algorithm.
    #[error("verkey code {verkey} incompatible with signature code {signature}")]
    IncompatibleCodes {
        /// CESR verkey code name.
        verkey: String,
        /// CESR signature code name.
        signature: String,
    },
}

/// Error returned by signature verification.
///
/// The verifying-key algorithm may be unavailable, the CESR codes may be
/// incompatible ([`CodeMismatchError`]), or the cryptographic check may fail
/// ([`SignatureError`]).
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    /// The cryptographic verification failed or the key/signature bytes were malformed.
    #[error(transparent)]
    Signature(#[from] SignatureError),

    /// The signature's CESR code does not belong to the verifying key's algorithm.
    #[error(transparent)]
    CodeMismatch(#[from] CodeMismatchError),

    /// The key has a recognized CESR code but this crate has no verifier for it.
    #[error("verification algorithm for {verkey:?} is unavailable")]
    UnsupportedAlgorithm {
        /// CESR verifying-key code that cannot be verified here.
        verkey: VerKeyCode,
    },
}

/// Failure verifying an indexed signature against a key list: either the signature's
/// index addressed no key, or the cryptographic check failed.
#[derive(Debug, thiserror::Error)]
pub enum IndexedVerifyError {
    /// The signature's key-index is beyond the end of the key list.
    #[error("signature index {index} out of range for {key_count} keys")]
    IndexOutOfRange {
        /// The out-of-range index carried by the signature.
        index: u32,
        /// The number of keys available to address.
        key_count: usize,
    },

    /// The resolved key did not verify the signature.
    #[error(transparent)]
    Verification(#[from] VerificationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_error_displays_signing_failed() {
        let err = SignatureError::SigningFailed("test".into());
        assert_eq!(err.to_string(), "signing failed: test");
    }

    #[test]
    fn signature_error_displays_invalid_length() {
        let err = SignatureError::InvalidSignatureLength {
            expected: 64,
            actual: 32,
        };
        assert!(err.to_string().contains("64"));
        assert!(err.to_string().contains("32"));
    }

    #[test]
    fn signature_error_displays_invalid() {
        let err = SignatureError::Invalid;
        assert_eq!(
            err.to_string(),
            "signature is invalid: does not match key and data"
        );
    }

    #[test]
    fn signature_error_displays_code_mismatch() {
        let err = SignatureError::CodeMismatch {
            expected: "Ed25519".into(),
            actual: "ECDSA256k1".into(),
        };
        assert_eq!(
            err.to_string(),
            "signature code ECDSA256k1 does not match key pair algorithm Ed25519"
        );
    }

    #[test]
    fn code_mismatch_error_displays_incompatible() {
        let err = CodeMismatchError::IncompatibleCodes {
            verkey: "Ed25519".into(),
            signature: "ECDSA256k1Sig".into(),
        };
        assert!(err.to_string().contains("Ed25519"));
        assert!(err.to_string().contains("ECDSA256k1Sig"));
    }

    // ===== Additional Display coverage =====

    #[test]
    fn signature_error_displays_verification_failed() {
        let err = SignatureError::VerificationFailed("bad sig".into());
        assert!(err.to_string().contains("bad sig"));
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn signature_error_debug_format() {
        let err = SignatureError::SigningFailed("debug test".into());
        let debug = format!("{err:?}");
        assert!(debug.contains("SigningFailed"));
        assert!(debug.contains("debug test"));
    }

    #[test]
    fn code_mismatch_error_debug_format() {
        let err = CodeMismatchError::IncompatibleCodes {
            verkey: "Ed25519".into(),
            signature: "ECDSA256k1Sig".into(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("IncompatibleCodes"));
    }

    #[test]
    fn verification_error_from_signature_error() {
        let e: VerificationError = SignatureError::Invalid.into();
        assert!(matches!(
            e,
            VerificationError::Signature(SignatureError::Invalid)
        ));
    }

    #[test]
    fn verification_error_from_code_mismatch() {
        let cm = CodeMismatchError::IncompatibleCodes {
            verkey: "Ed25519".into(),
            signature: "ECDSA256k1Sig".into(),
        };
        let e: VerificationError = cm.into();
        assert!(matches!(
            e,
            VerificationError::CodeMismatch(CodeMismatchError::IncompatibleCodes { .. })
        ));
    }

    #[test]
    fn verification_error_code_mismatch_display_is_transparent() {
        let cm = CodeMismatchError::IncompatibleCodes {
            verkey: "Ed25519".into(),
            signature: "ECDSA256k1Sig".into(),
        };
        let e: VerificationError = cm.into();
        assert_eq!(
            e.to_string(),
            CodeMismatchError::IncompatibleCodes {
                verkey: "Ed25519".into(),
                signature: "ECDSA256k1Sig".into(),
            }
            .to_string()
        );
    }

    #[test]
    fn verification_error_display_is_transparent() {
        let e: VerificationError = SignatureError::Invalid.into();
        assert_eq!(e.to_string(), SignatureError::Invalid.to_string());
    }
}
