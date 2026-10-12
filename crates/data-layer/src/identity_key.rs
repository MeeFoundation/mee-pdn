//! The identity key pair and everything derived from it or signed by
//! it, each under a context string of its own, all of them here;
//! the steps are the pod stores spec's.

use iroh::{PublicKey, SecretKey, Signature};
use pdn_store::NamespaceId;
use pdn_types::{PdnId, PodId};

use crate::pod::{
    encode_devices, CreatedPayload, DevicesPayload, JoinedPayload, MemberDevice, Seq,
};

const PDN_ID_CONTEXT: &str = "pdn/pdn-id/v1";
const POD_ID_CONTEXT: &str = "pdn/pod-id/v1";
const POD_CREATION_CONTEXT: &[u8] = b"pdn/pod-creation/v1";
const POD_JOIN_CONTEXT: &[u8] = b"pdn/pod-join/v1";
const POD_DEVICES_CONTEXT: &[u8] = b"pdn/pod-devices/v1";

/// The identity key pair: one per identity, minted with it, its public
/// key deriving the identity's `PdnId`.
#[derive(Clone, Debug)]
pub struct IdentityKeyPair(SecretKey);

impl IdentityKeyPair {
    pub fn generate() -> Self {
        Self(SecretKey::generate())
    }

    pub fn public_key(&self) -> [u8; 32] {
        *self.0.public().as_bytes()
    }

    pub fn pdn_id(&self) -> PdnId {
        pdn_id_of(&self.public_key())
    }

    /// The pair follows from these 32 bytes, which the PMS stores.
    pub(crate) fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        Self(SecretKey::from_bytes(secret))
    }

    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The created event of the pod `pod_id_of(self.pdn_id(), key, nonce)`,
    /// held in `membership_store` beside `record_store`.
    pub fn creation(
        &self,
        nonce: [u8; 16],
        membership_store: NamespaceId,
        record_store: NamespaceId,
    ) -> CreatedPayload {
        let identity_key = self.public_key();
        let mut payload = CreatedPayload {
            nonce,
            identity_key,
            membership_store,
            record_store,
            signature: [0; 64],
        };
        payload.signature = self
            .0
            .sign(&creation_message(&self.pdn_id(), &payload))
            .to_bytes();
        payload
    }

    /// The join statement over `subject_seq`, the sequence the inviting
    /// device names for this identity's joined event, and `device`, the one
    /// it joins from.
    pub fn join_statement(
        &self,
        pod: &PodId,
        subject_seq: Seq,
        device: MemberDevice,
    ) -> JoinedPayload {
        let identity_key = self.public_key();
        let message = join_message(&self.pdn_id(), &identity_key, pod, subject_seq, &device);
        JoinedPayload {
            identity_key,
            device,
            signature: self.0.sign(&message).to_bytes(),
        }
    }

    /// The device-list statement in `pod` at `version`, the version its key
    /// names.
    pub fn device_statement(
        &self,
        pod: &PodId,
        version: u64,
        devices: Vec<MemberDevice>,
    ) -> DevicesPayload {
        let message = devices_message(pod, &self.pdn_id(), version, &devices);
        DevicesPayload {
            devices,
            signature: self.0.sign(&message).to_bytes(),
        }
    }
}

/// Whether `payload`'s signature verifies for a created event in
/// `creator`'s chain; what its fields derive is the membership view's to
/// check.
pub(crate) fn creation_verifies(creator: &PdnId, payload: &CreatedPayload) -> bool {
    let message = creation_message(creator, payload);
    verifies(&payload.identity_key, &message, &payload.signature)
}

/// Whether `payload` is `pod`'s created event: its fields derive `pod`, and
/// its signature verifies under the key it carries, whose `PdnId` is the
/// creator's.
pub fn creates(pod: &PodId, payload: &CreatedPayload) -> bool {
    let creator = pdn_id_of(&payload.identity_key);
    pod_id_of(&creator, &payload.identity_key, &payload.nonce) == *pod
        && creation_verifies(&creator, payload)
}

/// Whether `payload`'s signature verifies for a joined event at
/// `subject_seq` of `subject`'s chain in `pod`.
pub fn join_verifies(
    subject: &PdnId,
    pod: &PodId,
    subject_seq: Seq,
    payload: &JoinedPayload,
) -> bool {
    let message = join_message(
        subject,
        &payload.identity_key,
        pod,
        subject_seq,
        &payload.device,
    );
    verifies(&payload.identity_key, &message, &payload.signature)
}

/// Whether `payload`'s signature verifies under `identity_key` for
/// `member`'s statement in `pod` at `version`.
pub fn devices_verify(
    identity_key: &[u8; 32],
    pod: &PodId,
    member: &PdnId,
    version: u64,
    payload: &DevicesPayload,
) -> bool {
    let message = devices_message(pod, member, version, &payload.devices);
    verifies(identity_key, &message, &payload.signature)
}

fn creation_message(creator: &PdnId, payload: &CreatedPayload) -> Vec<u8> {
    [
        POD_CREATION_CONTEXT,
        creator.as_bytes(),
        &payload.identity_key,
        &payload.nonce,
        payload.membership_store.as_bytes(),
        payload.record_store.as_bytes(),
    ]
    .concat()
}

fn join_message(
    subject: &PdnId,
    identity_key: &[u8; 32],
    pod: &PodId,
    subject_seq: Seq,
    device: &MemberDevice,
) -> Vec<u8> {
    [
        POD_JOIN_CONTEXT,
        subject.as_bytes(),
        identity_key,
        pod.as_bytes(),
        &subject_seq.get().to_be_bytes(),
        &encode_devices(std::slice::from_ref(device)),
    ]
    .concat()
}

fn devices_message(pod: &PodId, member: &PdnId, version: u64, devices: &[MemberDevice]) -> Vec<u8> {
    [
        POD_DEVICES_CONTEXT,
        pod.as_bytes(),
        member.as_bytes(),
        &version.to_be_bytes(),
        &encode_devices(devices),
    ]
    .concat()
}

/// Strict verification: a key that is no curve point verifies nothing.
fn verifies(identity_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    PublicKey::from_bytes(identity_key).is_ok_and(|key| {
        key.verify(message, &Signature::from_bytes(signature))
            .is_ok()
    })
}

/// The `PdnId` a public identity key derives.
pub fn pdn_id_of(identity_key: &[u8; 32]) -> PdnId {
    PdnId::from_bytes(blake3::derive_key(PDN_ID_CONTEXT, identity_key))
}

/// The pod id a created event's fields derive.
pub fn pod_id_of(creator: &PdnId, identity_key: &[u8; 32], nonce: &[u8; 16]) -> PodId {
    let mut hasher = blake3::Hasher::new_derive_key(POD_ID_CONTEXT);
    hasher.update(creator.as_bytes());
    hasher.update(identity_key);
    hasher.update(nonce);
    // The output stream's first 16 bytes are the hash's first 16.
    let mut id = [0u8; 16];
    hasher.finalize_xof().fill(&mut id);
    PodId::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alice's public identity key in the specs' examples.
    const ALICE_KEY: &str = "17cb79fb2b4120f2b1ec65e4198d6e08b28e813feb01e4a400839b85e18080ce";
    /// The second identity key of the specs' examples.
    const OTHER_KEY: &str = "d759793bbc13a2819a827c76adb6fba8a49aee007f49f2d0992d99b825ad2c48";

    fn key(hex: &str) -> [u8; 32] {
        pdn_types::parse_hex(hex).unwrap()
    }

    /// The key pair whose secret is 32 bytes of `33` has the public key the
    /// specs print for Alice and derives the `PdnId` they print for her.
    #[test]
    fn the_specs_example_key_pair_derives_alices_pdn_id() {
        let pair = IdentityKeyPair::from_secret_bytes(&[0x33; 32]);
        assert_eq!(pair.public_key(), key(ALICE_KEY));
        assert_eq!(
            pair.pdn_id().to_string(),
            "65bcff20d2b149925daa94e3750937044e8ef27385d24cb6cb7bf4182b408ba5"
        );
    }

    /// The namespace ids the specs print for Family's two stores.
    fn family_stores() -> (NamespaceId, NamespaceId) {
        (
            NamespaceId::from(&[0x4d; 32]),
            NamespaceId::from(&[0x52; 32]),
        )
    }

    /// Alice's created event for "Family" carries the signature the pod
    /// stores spec prints, and its fields derive the pod's id.
    #[test]
    fn the_specs_created_event_signs_and_derives_family() {
        let alice = IdentityKeyPair::from_secret_bytes(&[0x33; 32]);
        let (membership, records) = family_stores();
        let creation = alice.creation([0x5a; 16], membership, records);
        let head: [u8; 8] = pdn_types::parse_hex("81532fb40a10f1a4").unwrap();
        let tail: [u8; 4] = pdn_types::parse_hex("7c050a01").unwrap();
        assert!(creation.signature.starts_with(&head));
        assert!(creation.signature.ends_with(&tail));
        let family =
            PodId::from_bytes(pdn_types::parse_hex("ad58a3faa04cdc5576c8dc5823a347c6").unwrap());
        assert!(creates(&family, &creation));
        // Denied: the same event claimed for another creator's chain, for
        // another pod, and over stores it does not name.
        let other = pdn_id_of(&key(OTHER_KEY));
        assert!(!creation_verifies(&other, &creation));
        assert!(!creates(&PodId::from_bytes([0x0b; 16]), &creation));
        let moved = CreatedPayload {
            record_store: NamespaceId::from(&[0xee; 32]),
            ..creation
        };
        assert!(!creates(&family, &moved));
    }

    /// A join statement verifies at the sequence it signs, in its pod and for
    /// its device alone: one copied from an earlier join verifies at no later
    /// sequence, and another device in it verifies at none.
    #[test]
    fn a_join_statement_verifies_at_its_own_sequence_only() {
        let carol = IdentityKeyPair::generate();
        let family = PodId::from_bytes([0x9c; 16]);
        let device = MemberDevice {
            node: pdn_types::NodeId::from_bytes([0xc1; 32]),
            author: pdn_store::AuthorId::from(&[0xc1; 32]),
        };
        let statement = carol.join_statement(&family, Seq::new(1), device);
        assert!(join_verifies(
            &carol.pdn_id(),
            &family,
            Seq::new(1),
            &statement
        ));
        // Denied: the copy at a later sequence, in another pod, for another
        // member, and with another device in it.
        assert!(!join_verifies(
            &carol.pdn_id(),
            &family,
            Seq::new(3),
            &statement
        ));
        let wedding = PodId::from_bytes([0xf9; 16]);
        assert!(!join_verifies(
            &carol.pdn_id(),
            &wedding,
            Seq::new(1),
            &statement
        ));
        let dave = IdentityKeyPair::generate().pdn_id();
        assert!(!join_verifies(&dave, &family, Seq::new(1), &statement));
        let swapped = JoinedPayload {
            device: MemberDevice {
                node: pdn_types::NodeId::from_bytes([0xe1; 32]),
                author: pdn_store::AuthorId::from(&[0xe9; 32]),
            },
            ..statement
        };
        assert!(!join_verifies(
            &carol.pdn_id(),
            &family,
            Seq::new(1),
            &swapped
        ));
    }

    /// A device statement verifies under its member's key, in its pod, for
    /// its member, at its own version; moved to another version, pod or
    /// member, or checked under another key, it does not.
    #[test]
    fn a_device_statement_verifies_under_its_key_at_its_version_only() {
        let bob = IdentityKeyPair::generate();
        let family = PodId::from_bytes([0x9c; 16]);
        let devices = vec![MemberDevice {
            node: pdn_types::NodeId::from_bytes([0xb1; 32]),
            author: pdn_store::AuthorId::from(&[0xa1; 32]),
        }];
        let statement = bob.device_statement(&family, 2, devices);
        let key = bob.public_key();
        assert!(devices_verify(&key, &family, &bob.pdn_id(), 2, &statement));
        // Denied: the statement at another version, in another pod, for
        // another member, and under another key.
        assert!(!devices_verify(&key, &family, &bob.pdn_id(), 3, &statement));
        let wedding = PodId::from_bytes([0xf9; 16]);
        assert!(!devices_verify(
            &key,
            &wedding,
            &bob.pdn_id(),
            2,
            &statement
        ));
        let dave = IdentityKeyPair::generate();
        assert!(!devices_verify(
            &key,
            &family,
            &dave.pdn_id(),
            2,
            &statement
        ));
        assert!(!devices_verify(
            &dave.public_key(),
            &family,
            &bob.pdn_id(),
            2,
            &statement
        ));
    }

    /// Alice's key and her `PdnId` derive the ids the specs print for "Family"
    /// and her second pod, one per nonce.
    #[test]
    fn pod_id_derives_from_creator_key_and_nonce() {
        let alice_key = key(ALICE_KEY);
        let alice = pdn_id_of(&alice_key);
        assert_eq!(
            pod_id_of(&alice, &alice_key, &[0x5a; 16]).to_string(),
            "ad58a3faa04cdc5576c8dc5823a347c6"
        );
        assert_eq!(
            pod_id_of(&alice, &alice_key, &[0xa5; 16]).to_string(),
            "920657b318e1bb314e4cb2cf2174451a"
        );
    }

    /// A created event under another key derives another pod id, whether it
    /// names Alice's `PdnId` or the one that key derives.
    #[test]
    fn another_key_derives_another_pod_id() {
        let alice = pdn_id_of(&key(ALICE_KEY));
        let other_key = key(OTHER_KEY);
        let other = pdn_id_of(&other_key);
        assert_eq!(
            pod_id_of(&alice, &other_key, &[0x5a; 16]).to_string(),
            "0b1c51caa1cc711a3079f791899c43a7"
        );
        assert_eq!(
            pod_id_of(&other, &other_key, &[0x5a; 16]).to_string(),
            "4470d82a3f3ecc18d9e0889a23f98fed"
        );
    }
}
