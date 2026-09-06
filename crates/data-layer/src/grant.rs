//! The minimal grant: one issuer grants one audience read — and, per
//! claim, write — on an exact set of claims. Unsigned, so a serving node
//! trusts only its own recorded copy, never one presented over the wire.

use std::sync::OnceLock;

use pdn_types::{ClaimId, EntryPath, NonEmpty, PdnId};
use serde::{Deserialize, Serialize};

/// Domain-separation context for the claim-identity derivation, versioned
/// in the string itself.
const CLAIM_ID_CONTEXT: &str = "pdn.claim-id.v0";

/// A hasher with the context already absorbed, cloned per derivation: the
/// egress filter derives on every entry a range scan touches, and cloning
/// measures ~2.35× faster than absorbing the context each time.
fn context_hasher() -> blake3::Hasher {
    static BASE: OnceLock<blake3::Hasher> = OnceLock::new();
    BASE.get_or_init(|| blake3::Hasher::new_derive_key(CLAIM_ID_CONTEXT))
        .clone()
}

/// The claim identity of the entry at `path` in `issuer`'s data namespace:
/// derived from the location, so it is stable under payload edits and
/// computable from an entry key alone. Two accepted costs: the id follows
/// the path (a relocation changes it), and it is invertible by a dictionary
/// search over paths.
pub fn claim_id_of(issuer: &PdnId, path: &EntryPath) -> ClaimId {
    claim_id_of_key(issuer, path.as_str().as_bytes())
}

/// [`claim_id_of`] over a raw entry key — the one derivation the grant, the
/// egress filter, and the ingest gate all go through. A key that is not a
/// valid path derives an id no grant contains, so the filters exclude it
/// without allocating an [`EntryPath`] per entry. The issuer is fixed-width,
/// so the concatenation is injective without a separator.
pub(crate) fn claim_id_of_key(issuer: &PdnId, key: &[u8]) -> ClaimId {
    let mut hasher = context_hasher();
    hasher.update(issuer.as_bytes());
    hasher.update(key);
    ClaimId::from_bytes(*hasher.finalize().as_bytes())
}

/// One granted claim: read always, write when `write` — `UWill`'s per-claim
/// command list, compressed to the one optional command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantedClaim {
    pub claim: ClaimId,
    pub write: bool,
}

/// A single grant: `issuer` grants `audience` read — and, per claim, write
/// — on exactly `claims` within the issuer's data namespace. Serialized as
/// JSON inside the grant record; self-contained (it repeats the issuer the
/// record key names) because the store treats the payload as opaque bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadGrant {
    pub issuer: PdnId,
    pub audience: PdnId,
    /// Exact claims, no prefixes or other geometry.
    pub claims: NonEmpty<GrantedClaim>,
}

impl ReadGrant {
    pub fn covers_read(&self, path: &EntryPath) -> bool {
        let id = claim_id_of(&self.issuer, path);
        self.claims.iter().any(|granted| granted.claim == id)
    }

    pub fn covers_write(&self, path: &EntryPath) -> bool {
        let id = claim_id_of(&self.issuer, path);
        self.claims
            .iter()
            .any(|granted| granted.write && granted.claim == id)
    }

    /// Decides the ticket mode the grant ships (`ShareMode`).
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

    /// Fail-closed: a flat claim list with one grant-wide write flag reads
    /// as no grant, not as some default width.
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
