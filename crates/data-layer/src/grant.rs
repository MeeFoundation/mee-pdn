//! The minimal grant: one issuer grants one audience read — and, per
//! claim, write — on an exact set of claims. No delegation chain, no
//! revocation cryptography, no token encoding. A serving node trusts only
//! its own recorded copy of a grant, never one presented over the wire.

use std::sync::OnceLock;

use pdn_types::{ClaimId, EntryPath, NonEmpty, PdnId};
use serde::{Deserialize, Serialize};

/// Domain-separation context for the claim-identity derivation, versioned
/// in the string itself.
const CLAIM_ID_CONTEXT: &str = "pdn.claim-id.v0";

/// A hasher with the domain-separation context already absorbed, cloned per
/// derivation. Absorbing the context is the expensive, unchanging prefix of
/// every derivation, so it is paid once — the egress filter derives on
/// every entry a range scan touches and the fork requires it cheap
/// (cloning measures ~2.35× faster than re-deriving).
fn context_hasher() -> blake3::Hasher {
    static BASE: OnceLock<blake3::Hasher> = OnceLock::new();
    BASE.get_or_init(|| blake3::Hasher::new_derive_key(CLAIM_ID_CONTEXT))
        .clone()
}

/// The claim identity of the entry at `path` in `issuer`'s data namespace,
/// derived from the storage location: stable under payload edits — a grant
/// survives editing the shared claim — and computable from an entry key
/// alone, which lets the egress filter evaluate membership with no
/// id-to-location mapping (key → id → granted set). Two accepted costs:
/// the id follows the path (an entry relocation changes it), and the id is
/// invertible by a dictionary search over paths.
pub fn claim_id_of(issuer: &PdnId, path: &EntryPath) -> ClaimId {
    claim_id_of_key(issuer, path.as_str().as_bytes())
}

/// [`claim_id_of`] over a raw entry key — the one derivation the grant
/// covers, the egress filter's per-entry test, and the ingest gate's
/// per-entry test all go through. A valid path's key bytes are exactly its
/// string bytes, so the two forms agree wherever both are defined; a key
/// that is not a valid path derives an id no grant contains (granted ids
/// are only ever minted from valid paths), so the filters exclude it
/// without allocating an [`EntryPath`] per entry.
///
/// The issuer is fixed-width (32 bytes), so the concatenation with the key
/// bytes is injective without a separator.
pub(crate) fn claim_id_of_key(issuer: &PdnId, key: &[u8]) -> ClaimId {
    let mut hasher = context_hasher();
    hasher.update(issuer.as_bytes());
    hasher.update(key);
    ClaimId::from_bytes(*hasher.finalize().as_bytes())
}

/// One granted claim: its identity, and whether write is granted alongside
/// the always-present read — `UWill`'s per-claim command list, compressed to
/// the one optional command this grant can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantedClaim {
    /// The claim identity.
    pub claim: ClaimId,
    /// Whether write is granted alongside read.
    pub write: bool,
}

/// A single grant: `issuer` grants `audience` read — and, per claim, write
/// — on exactly the claims in `claims`, within the issuer's data
/// namespace.
///
/// Read is always granted per claim (it is all the egress filter
/// consumes); write is enforced per claim by the ingest gate at the
/// issuer's devices. The record's ticket follows its commands as a whole:
/// any write ships a write ticket — the namespace secret is what carries
/// write authority over the wire, and the gate is what scopes it.
///
/// Serialized as JSON inside the grant record of the connection metadata
/// store; the payload is self-contained (it repeats the issuer the record
/// key names) because grant payloads stay opaque bytes to the store layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadGrant {
    /// Whose data namespace the grant opens.
    pub issuer: PdnId,
    /// The identity the grant is issued to.
    pub audience: PdnId,
    /// The exact claims granted — no prefixes or other geometry, mirroring
    /// `UWill`'s point-wise resource model — each carrying its own
    /// commands.
    pub claims: NonEmpty<GrantedClaim>,
}

impl ReadGrant {
    /// Whether this grant covers reading the entry at `path` — evaluated
    /// in the reverse direction: derive the claim identity from the key,
    /// test membership in the granted set.
    pub fn covers_read(&self, path: &EntryPath) -> bool {
        let id = claim_id_of(&self.issuer, path);
        self.claims.iter().any(|granted| granted.claim == id)
    }

    /// Whether this grant covers writing the entry at `path` — membership
    /// among the claims that carry write.
    pub fn covers_write(&self, path: &EntryPath) -> bool {
        let id = claim_id_of(&self.issuer, path);
        self.claims
            .iter()
            .any(|granted| granted.write && granted.claim == id)
    }

    /// Whether any claim of this grant carries write — what decides the
    /// ticket mode the grant ships (`ShareMode`).
    pub fn grants_any_write(&self) -> bool {
        self.claims.iter().any(|granted| granted.write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> EntryPath {
        EntryPath::new(s).unwrap()
    }

    fn read_on(issuer: &PdnId, p: &EntryPath) -> GrantedClaim {
        GrantedClaim {
            claim: claim_id_of(issuer, p),
            write: false,
        }
    }

    fn write_on(issuer: &PdnId, p: &EntryPath) -> GrantedClaim {
        GrantedClaim {
            claim: claim_id_of(issuer, p),
            write: true,
        }
    }

    #[test]
    fn claim_id_is_stable_and_distinguishes_issuer_and_path() {
        let issuer_a = PdnId::from_bytes([0xa1; 32]);
        let issuer_b = PdnId::from_bytes([0xb0; 32]);
        let email = path("contact/email");
        let phone = path("contact/phone");

        assert_eq!(
            claim_id_of(&issuer_a, &email),
            claim_id_of(&issuer_a, &email)
        );
        assert_ne!(
            claim_id_of(&issuer_a, &email),
            claim_id_of(&issuer_a, &phone)
        );
        assert_ne!(
            claim_id_of(&issuer_a, &email),
            claim_id_of(&issuer_b, &email)
        );
    }

    #[test]
    fn grant_covers_exactly_its_claims_with_their_commands() {
        let issuer = PdnId::from_bytes([0xa1; 32]);
        let audience = PdnId::from_bytes([0xb0; 32]);
        let email = path("contact/email");
        let phone = path("contact/phone");
        let mut claims = NonEmpty::new(read_on(&issuer, &email));
        claims.push(write_on(&issuer, &phone));
        let grant = ReadGrant {
            issuer,
            audience,
            claims,
        };
        // Read covers both granted claims and nothing else.
        assert!(grant.covers_read(&email));
        assert!(grant.covers_read(&phone));
        assert!(!grant.covers_read(&path("notes/diary")));
        // Write covers exactly the claim that carries it.
        assert!(!grant.covers_write(&email));
        assert!(grant.covers_write(&phone));
        assert!(grant.grants_any_write());
        // The same path under another issuer is a different claim.
        let other = ReadGrant {
            issuer: audience,
            ..grant
        };
        assert!(!other.covers_read(&email));
    }

    #[test]
    fn a_grant_without_write_grants_none() {
        let issuer = PdnId::from_bytes([0xa1; 32]);
        let email = path("contact/email");
        let grant = ReadGrant {
            issuer,
            audience: PdnId::from_bytes([0xb0; 32]),
            claims: NonEmpty::new(read_on(&issuer, &email)),
        };
        assert!(!grant.grants_any_write());
        assert!(!grant.covers_write(&email));
    }

    #[test]
    fn grant_serde_round_trips() {
        let issuer = PdnId::from_bytes([0xa1; 32]);
        let mut claims = NonEmpty::new(read_on(&issuer, &path("contact/email")));
        claims.push(write_on(&issuer, &path("contact/phone")));
        let grant = ReadGrant {
            issuer,
            audience: PdnId::from_bytes([0xb0; 32]),
            claims,
        };
        let json = serde_json::to_string(&grant).unwrap();
        let back: ReadGrant = serde_json::from_str(&json).unwrap();
        assert_eq!(back, grant);
    }

    /// A payload carrying a flat claim list with one grant-wide write flag
    /// fails to decode — fail-closed: an unknown shape reads as no grant
    /// rather than as some default width.
    #[test]
    fn a_flat_claims_payload_decodes_as_no_grant() {
        let issuer = PdnId::from_bytes([0xa1; 32]);
        let claim = claim_id_of(&issuer, &path("contact/email"));
        let flat = format!(
            r#"{{"issuer":"{issuer}","audience":"{audience}","claims":["{claim}"],"write":true}}"#,
            audience = PdnId::from_bytes([0xb0; 32]),
        );
        assert!(serde_json::from_str::<ReadGrant>(&flat).is_err());
    }
}
