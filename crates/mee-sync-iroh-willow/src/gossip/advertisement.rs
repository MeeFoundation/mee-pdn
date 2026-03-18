use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::GossipError;

/// Advertisement broadcast over the gossip discovery topic.
///
/// Contains the advertising peer's identity, network endpoints,
/// and the namespaces they serve. Receivers intersect namespace
/// IDs against their held capabilities to find sync targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAdvertisement {
    // TODO(keri): Should be AID bytes (root identifier), not SubspaceId.
    /// Peer identity (`SubspaceId` bytes now; AID after KERI).
    pub peer_id: [u8; 32],
    /// iroh EndpointId(s) for address resolution.
    pub endpoint_ids: Vec<[u8; 32]>,
    // TODO(bloom-filter): Replace plain namespace list with bloom filter:
    // namespace_filter: Vec<u8>, filter_num_bits: u32, filter_num_hashes: u8.
    // Update compute_signature() and serialization accordingly.
    /// Namespace IDs this node serves (plain list).
    /// Replaced by bloom filter in Phase 1b.
    pub namespace_ids: Vec<[u8; 32]>,
    /// Direct addresses ("ip:port") for peer connectivity.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Unix epoch seconds. Used for both ordering (newer ad wins)
    /// and staleness detection.
    pub timestamp: u64,
    // TODO(keri): Replace SHA256 placeholder with Ed25519 signature
    // using the operational private key from the signer's KEL.
    /// Placeholder signature (SHA256 hash of content).
    pub signature: [u8; 32],
}

impl PeerAdvertisement {
    /// Compute the placeholder SHA256 signature over the
    /// content fields (excludes the signature field itself).
    pub fn compute_signature(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.peer_id);
        let mut sorted_eids = self.endpoint_ids.clone();
        sorted_eids.sort_unstable();
        for eid in &sorted_eids {
            hasher.update(eid);
        }
        let mut sorted_ns = self.namespace_ids.clone();
        sorted_ns.sort_unstable();
        for ns in &sorted_ns {
            hasher.update(ns);
        }
        let mut sorted_addrs = self.addresses.clone();
        sorted_addrs.sort_unstable();
        for addr in &sorted_addrs {
            hasher.update(addr.as_bytes());
        }
        hasher.update(self.timestamp.to_le_bytes());
        hasher.finalize().into()
    }

    /// Set the signature field by computing it.
    pub fn sign(&mut self) {
        self.signature = self.compute_signature();
    }

    /// Verify the signature matches the content.
    pub fn verify_signature(&self) -> bool {
        self.signature == self.compute_signature()
    }

    /// Serialize to bytes for gossip broadcast.
    pub fn to_bytes(&self) -> Result<Bytes, GossipError> {
        let data = serde_json::to_vec(self)?;
        Ok(Bytes::from(data))
    }

    /// Deserialize from received gossip message bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, GossipError> {
        Ok(serde_json::from_slice(data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ad() -> PeerAdvertisement {
        let mut ad = PeerAdvertisement {
            peer_id: [1u8; 32],
            endpoint_ids: vec![[2u8; 32]],
            namespace_ids: vec![[3u8; 32], [4u8; 32]],
            addresses: vec![],
            timestamp: 1_700_000_000,
            signature: [0u8; 32],
        };
        ad.sign();
        ad
    }

    #[test]
    fn advertisement_roundtrip_serde() {
        let ad = sample_ad();
        let bytes = ad.to_bytes().expect("serialize");
        let decoded = PeerAdvertisement::from_bytes(&bytes).expect("deserialize");
        assert_eq!(ad, decoded);
    }

    #[test]
    fn advertisement_empty_namespaces() {
        let mut ad = PeerAdvertisement {
            peer_id: [1u8; 32],
            endpoint_ids: vec![[2u8; 32]],
            namespace_ids: vec![],
            addresses: vec![],
            timestamp: 0,
            signature: [0u8; 32],
        };
        ad.sign();
        assert!(ad.verify_signature());
        let bytes = ad.to_bytes().expect("serialize");
        let decoded = PeerAdvertisement::from_bytes(&bytes).expect("deserialize");
        assert_eq!(ad, decoded);
    }

    #[test]
    fn signature_valid() {
        let ad = sample_ad();
        assert!(ad.verify_signature());
    }

    #[test]
    fn signature_tampered() {
        let mut ad = sample_ad();
        // Tamper with namespace list
        ad.namespace_ids.push([99u8; 32]);
        assert!(!ad.verify_signature());
    }

    #[test]
    fn signature_tampered_timestamp() {
        let mut ad = sample_ad();
        ad.timestamp = 999;
        assert!(!ad.verify_signature());
    }
}
