//! The party a replica is held for, and the party a session's caller acts
//! for: 32 bytes this crate compares and never interprets. A consumer fills
//! them with whatever identifies a party in its own vocabulary.

use serde::{Deserialize, Serialize};

/// Whose replica a session addresses, and whom its caller acts for.
///
/// Opaque here: equality is the only question this crate asks of it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Holder([u8; 32]);

impl Holder {
    /// Wrap the consumer's 32-byte identifier.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The wrapped bytes, as the consumer handed them in.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first few hex characters, for logs.
    pub fn fmt_short(&self) -> String {
        hex::encode(&self.0[..5])
    }
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl std::fmt::Debug for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Holder({})", self.fmt_short())
    }
}

impl From<[u8; 32]> for Holder {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// A peer to dial for one replica, paired with the holder it is dialed as.
///
/// The pairing is what lets a node addressed by several of its holders be
/// reached at the right one: the address alone names a node, and a node
/// hosts any number of holders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Where the peer is reached.
    pub addr: iroh::EndpointAddr,
    /// Whose replica is addressed there.
    pub holder: Holder,
}

impl Contact {
    /// Pair an address with the holder it is dialed as.
    pub fn new(addr: iroh::EndpointAddr, holder: Holder) -> Self {
        Self { addr, holder }
    }
}
