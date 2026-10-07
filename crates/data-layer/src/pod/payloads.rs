//! The membership payloads the fold reads, each a run of fixed-size fields
//! in a fixed order, decoded only at its exact length. What each signs is
//! `announcement`'s.

use pdn_store::AuthorId;
use pdn_types::NodeId;

/// A left, removed, promoted or demoted event's payload: its key carries
/// all of the event, and an empty entry is a tombstone.
pub const ACT_PAYLOAD: [u8; 1] = [0];

/// A device of a member: the node sessions are classified and contacts
/// dialed by, and the author the member writes with there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberDevice {
    pub node: NodeId,
    pub author: AuthorId,
}

/// The founding event's payload: `nonce ‖ announcement_key ‖ signature`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundedPayload {
    pub nonce: [u8; 16],
    pub announcement_key: [u8; 32],
    pub signature: [u8; 64],
}

/// A joined event's payload, the newcomer's join statement:
/// `announcement_key ‖ signature`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinedPayload {
    pub announcement_key: [u8; 32],
    pub signature: [u8; 64],
}

/// A device-list statement's payload: `(node ‖ author)… ‖ signature`. Its
/// version is its key's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicesPayload {
    pub devices: Vec<MemberDevice>,
    pub signature: [u8; 64],
}

impl FoundedPayload {
    pub fn encode(&self) -> Vec<u8> {
        [&self.nonce[..], &self.announcement_key, &self.signature].concat()
    }

    pub fn decode(mut bytes: &[u8]) -> Option<Self> {
        let payload = Self {
            nonce: take(&mut bytes)?,
            announcement_key: take(&mut bytes)?,
            signature: take(&mut bytes)?,
        };
        bytes.is_empty().then_some(payload)
    }
}

impl JoinedPayload {
    pub fn encode(&self) -> Vec<u8> {
        [&self.announcement_key[..], &self.signature].concat()
    }

    pub fn decode(mut bytes: &[u8]) -> Option<Self> {
        let payload = Self {
            announcement_key: take(&mut bytes)?,
            signature: take(&mut bytes)?,
        };
        bytes.is_empty().then_some(payload)
    }
}

impl DevicesPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = encode_devices(&self.devices);
        bytes.extend_from_slice(&self.signature);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (mut listed, signature) = bytes.split_last_chunk::<64>()?;
        let mut devices = Vec::new();
        while !listed.is_empty() {
            devices.push(MemberDevice {
                node: NodeId::from_bytes(take(&mut listed)?),
                author: AuthorId::from(&take::<32>(&mut listed)?),
            });
        }
        Some(Self {
            devices,
            signature: *signature,
        })
    }
}

/// `(node ‖ author)…`, the part of a device-list statement its signature
/// covers beside the version.
pub(crate) fn encode_devices(devices: &[MemberDevice]) -> Vec<u8> {
    devices
        .iter()
        .flat_map(|device| {
            device
                .node
                .as_bytes()
                .iter()
                .chain(device.author.as_bytes())
                .copied()
        })
        .collect()
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = bytes.split_first_chunk::<N>()?;
    *bytes = rest;
    Some(*head)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each payload decodes back to what encoded it, and a payload one
    /// byte short or long decodes to nothing.
    #[test]
    fn payloads_decode_at_their_exact_length_only() {
        let founded = FoundedPayload {
            nonce: [0x5a; 16],
            announcement_key: [0x17; 32],
            signature: [0x0e; 64],
        };
        let joined = JoinedPayload {
            announcement_key: [0x17; 32],
            signature: [0x0e; 64],
        };
        let devices = DevicesPayload {
            devices: vec![
                MemberDevice {
                    node: NodeId::from_bytes([0xb1; 32]),
                    author: AuthorId::from(&[0xa1; 32]),
                },
                MemberDevice {
                    node: NodeId::from_bytes([0xb2; 32]),
                    author: AuthorId::from(&[0xa2; 32]),
                },
            ],
            signature: [0x0e; 64],
        };

        let encoded = founded.encode();
        assert_eq!(encoded.len(), 112);
        assert_eq!(FoundedPayload::decode(&encoded), Some(founded));
        let encoded = joined.encode();
        assert_eq!(encoded.len(), 96);
        assert_eq!(JoinedPayload::decode(&encoded), Some(joined));
        let encoded = devices.encode();
        assert_eq!(encoded.len(), 2 * 64 + 64);
        assert_eq!(DevicesPayload::decode(&encoded), Some(devices.clone()));

        let mut long = devices.encode();
        long.push(0);
        assert_eq!(DevicesPayload::decode(&long), None);
        assert_eq!(FoundedPayload::decode(&[0; 111]), None);
        assert_eq!(FoundedPayload::decode(&[0; 113]), None);
        assert_eq!(JoinedPayload::decode(&[0; 95]), None);
        assert_eq!(DevicesPayload::decode(&[0; 63]), None);
    }
}
