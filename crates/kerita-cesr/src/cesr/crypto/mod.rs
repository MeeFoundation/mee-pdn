//! CESR-compatible cryptographic signing, verification, and digest primitives.

#[allow(
    unused_imports,
    reason = "alloc prelude items; subset used per cfg/feature combination"
)]
use alloc::{format, string::String, string::ToString, vec, vec::Vec};

/// Algorithm marker types and the `Algorithm` sealed trait.
pub mod algo;
/// Digest computation for all supported CESR hash algorithms.
pub mod digest;
/// Error types for signing, verification, key, and digest operations.
pub mod error;
/// Generic `KeyPair<A>` for Ed25519, secp256k1, and secp256r1.
/// Root salt material and argon2id seed stretching (K7 custody substrate).
/// The `Signature` trait unifying non-indexed (`Cigar`) and indexed (`Siger`)
/// signatures for a single generic `verify`.
pub mod signature;
/// Standalone signature verification dispatching on `VerKeyCode` at runtime.
pub mod verify;

// Re-exports for convenience
pub use algo::{Algorithm, Ed25519, Secp256r1};
pub use digest::digest;
pub use error::{
    CodeMismatchError, DigestError, IndexedVerifyError, SignatureError, VerificationError,
};
pub use signature::Signature;
pub use verify::{verify, verify_indexed};
