//! Whether a signature verifies: the key's algorithm and the signature's must agree, and an
//! ECDSA signature verifies with either S value, as keripy verifies it.

use crate::cesr::crypto::verify::{verify_ed25519, verify_secp256r1};
use crate::primitive::{IndexedSignature, Primitive};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Ed25519,
    P256,
}

fn key_algorithm(key: &Primitive) -> Option<Algorithm> {
    match key.code() {
        "D" | "B" => Some(Algorithm::Ed25519),
        "1AAJ" | "1AAI" => Some(Algorithm::P256),
        _ => None,
    }
}

fn verifies(algorithm: Option<Algorithm>, key: &Primitive, data: &[u8], signature: &[u8]) -> bool {
    match (key_algorithm(key), algorithm) {
        (Some(Algorithm::Ed25519), Some(Algorithm::Ed25519)) => {
            verify_ed25519(key.raw(), data, signature).is_ok()
        }
        (Some(Algorithm::P256), Some(Algorithm::P256)) => {
            verify_secp256r1(key.raw(), data, signature).is_ok()
        }
        _ => false,
    }
}

/// Whether `signature` is `key`'s over `data`.
pub fn indexed_verifies(key: &Primitive, data: &[u8], signature: &IndexedSignature) -> bool {
    let algorithm = match signature.code() {
        "A" | "B" | "2A" | "2B" => Some(Algorithm::Ed25519),
        "E" | "F" | "2E" | "2F" => Some(Algorithm::P256),
        _ => None,
    };
    verifies(algorithm, key, data, signature.raw())
}

/// Whether the plain signature `signature` is `key`'s over `data`.
pub fn plain_verifies(key: &Primitive, data: &[u8], signature: &Primitive) -> bool {
    let algorithm = match signature.code() {
        "0B" => Some(Algorithm::Ed25519),
        "0I" => Some(Algorithm::P256),
        _ => None,
    };
    verifies(algorithm, key, data, signature.raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer as _;

    /// An Ed25519 signature verifies under its own key, and not under a P-256 key, whatever the
    /// bytes of that key.
    #[test]
    fn a_signature_verifies_under_a_key_of_its_own_algorithm_alone() {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let key = Primitive::new("D", signer.verifying_key().as_bytes()).unwrap();
        let signature = Primitive::new("0B", &signer.sign(b"event").to_bytes()).unwrap();
        assert!(plain_verifies(&key, b"event", &signature));
        assert!(!plain_verifies(&key, b"other", &signature));
        let other = p256::ecdsa::SigningKey::from_slice(&[7; 32]).unwrap();
        let p256_key =
            Primitive::new("1AAJ", other.verifying_key().to_sec1_point(true).as_bytes()).unwrap();
        assert!(!plain_verifies(&p256_key, b"event", &signature));
    }
}
