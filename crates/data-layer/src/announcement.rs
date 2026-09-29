//! An identity's announcement key pair and everything derived from it or
//! signed by it, each under a context string of its own, all of them here;
//! the steps are the cell stores spec's.

use iroh::SecretKey;
use pdn_types::{CellId, PdnId};

const PDN_ID_CONTEXT: &str = "pdn/pdn-id/v1";
const CELL_ID_CONTEXT: &str = "pdn/cell-id/v1";

/// An identity's device-announcement key pair: one per identity, minted with
/// it, its public key deriving the identity's `PdnId`.
#[derive(Clone, Debug)]
pub struct AnnouncementKeyPair(SecretKey);

impl AnnouncementKeyPair {
    pub fn generate() -> Self {
        Self(SecretKey::generate())
    }

    pub fn public_key(&self) -> [u8; 32] {
        *self.0.public().as_bytes()
    }

    pub fn pdn_id(&self) -> PdnId {
        pdn_id_of(&self.public_key())
    }

    /// The pair follows from these 32 bytes, which the directory stores.
    pub(crate) fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        Self(SecretKey::from_bytes(secret))
    }

    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

/// The `PdnId` an announcement public key derives.
pub fn pdn_id_of(announcement_key: &[u8; 32]) -> PdnId {
    PdnId::from_bytes(blake3::derive_key(PDN_ID_CONTEXT, announcement_key))
}

/// The cell id a founding event's fields derive.
pub fn cell_id_of(creator: &PdnId, announcement_key: &[u8; 32], nonce: &[u8; 16]) -> CellId {
    let mut hasher = blake3::Hasher::new_derive_key(CELL_ID_CONTEXT);
    hasher.update(creator.as_bytes());
    hasher.update(announcement_key);
    hasher.update(nonce);
    // The output stream's first 16 bytes are the hash's first 16.
    let mut id = [0u8; 16];
    hasher.finalize_xof().fill(&mut id);
    CellId::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alice's announcement public key in the specs' examples.
    const ALICE_KEY: &str = "17cb79fb2b4120f2b1ec65e4198d6e08b28e813feb01e4a400839b85e18080ce";
    /// The second announcement key of the specs' examples.
    const OTHER_KEY: &str = "d759793bbc13a2819a827c76adb6fba8a49aee007f49f2d0992d99b825ad2c48";

    fn key(hex: &str) -> [u8; 32] {
        pdn_types::parse_hex(hex).unwrap()
    }

    /// The key pair whose secret is 32 bytes of `33` has the public key the
    /// specs print for Alice and derives the `PdnId` they print for her.
    #[test]
    fn the_specs_example_key_pair_derives_alices_pdn_id() {
        let pair = AnnouncementKeyPair::from_secret_bytes(&[0x33; 32]);
        assert_eq!(pair.public_key(), key(ALICE_KEY));
        assert_eq!(
            pair.pdn_id().to_string(),
            "65bcff20d2b149925daa94e3750937044e8ef27385d24cb6cb7bf4182b408ba5"
        );
    }

    /// Alice's key and her `PdnId` derive the ids the specs print for "Family"
    /// and her second cell, one per nonce.
    #[test]
    fn cell_id_derives_from_creator_key_and_nonce() {
        let alice_key = key(ALICE_KEY);
        let alice = pdn_id_of(&alice_key);
        assert_eq!(
            cell_id_of(&alice, &alice_key, &[0x5a; 16]).to_string(),
            "9cbcbe4da7cc35a44360d64e45621957"
        );
        assert_eq!(
            cell_id_of(&alice, &alice_key, &[0xa5; 16]).to_string(),
            "b61cdcf20d79379e475d57d1c04db7a4"
        );
    }

    /// A founding event under another key derives another cell id, whether it
    /// names Alice's `PdnId` or the one that key derives.
    #[test]
    fn another_key_derives_another_cell_id() {
        let alice = pdn_id_of(&key(ALICE_KEY));
        let other_key = key(OTHER_KEY);
        let other = pdn_id_of(&other_key);
        assert_eq!(
            cell_id_of(&alice, &other_key, &[0x5a; 16]).to_string(),
            "12849b66c608a62f31430f934efc083e"
        );
        assert_eq!(
            cell_id_of(&other, &other_key, &[0x5a; 16]).to_string(),
            "211891a43656908e3d5804f5c25a38f4"
        );
    }
}
