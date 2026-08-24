//! What crosses the binding boundary, and the parsing that decides a
//! malformed input before the runtime is called.
//!
//! Identities, claim identities and entry paths cross as text: a screen
//! shows them and a person compares them by eye. Entry payloads cross as
//! bytes with no framing of the facade's own — what was written is what is
//! read.
//!
//! A grant crosses as its capability alone. The runtime's read of a peer's
//! grant carries the replica's ticket beside it and the facade drops it
//! there: the sanctioned way a granted namespace arrives is that the
//! runtime binds what the grant names, and a caller able to import a
//! ticket would keep showing data after that binding broke.

use std::time::Duration;

use pdn_node::{claim_id_of, EntryInfo, EntryPath, GrantedClaim, NonEmpty, PdnId, ReadGrant};

use crate::error::PdnError;

/// The widest duration a caller may name for a lifetime or a budget.
/// Generous, and far below the point where adding it to the runtime's
/// current instant would overflow.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

/// The ceiling on one entry payload. A payload crosses the boundary as one
/// buffer in memory, and a phone is killed for memory rather than asked to
/// swap, so an unbounded one ends the process instead of returning an
/// error. `pdn-node-http` bounds its own at 64 MB for a container; a
/// device gets a tighter one.
///
/// It bounds what this host puts in and not what a read hands back: an
/// entry another node wrote — through a host whose own ceiling is 64 times
/// this one — is in the replica whatever its size, and refusing to hand it
/// over would make a claim the grant permits unreadable rather than making
/// the device safer. A caller that cares reads the length a listing
/// reports first.
pub const MAX_ENTRY_PAYLOAD: u64 = 1_048_576;

/// One claim of a grant publication, named by the path it covers.
///
/// A grant carries claim identities, derived one-way from the issuer and an
/// entry path, and the derivation happens here: every other exported call
/// addresses an entry by path, so a caller deriving identities itself would
/// be reproducing a rule of the product in order to talk to it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct GrantedPath {
    /// The entry path the claim covers.
    pub path: String,
    /// Whether write is granted alongside read.
    pub write: bool,
}

/// One claim of a grant as a read reports it: the derived identity, and
/// whether it carries write.
///
/// The derivation is one-way, so nothing recovers a path from this. A
/// caller that wants to show paths derives the identities of the paths it
/// knows or has listed ([`crate::claim_id`]) and joins them against what a
/// read reported.
#[derive(Debug, Clone, uniffi::Record)]
pub struct GrantedClaimId {
    /// The claim identity.
    pub claim: String,
    /// Whether write is granted alongside read.
    pub write: bool,
}

/// One grant's capability — issuer, audience, claims. No ticket, in either
/// direction: no exported call reports one and none accepts one.
#[derive(Debug, Clone, uniffi::Record)]
pub struct GrantCapability {
    /// Whose data namespace the grant opens.
    pub issuer: String,
    /// The identity the grant is issued to.
    pub audience: String,
    /// The exact claims granted, each with its own commands.
    pub claims: Vec<GrantedClaimId>,
}

impl From<ReadGrant> for GrantCapability {
    fn from(grant: ReadGrant) -> Self {
        Self {
            issuer: grant.issuer.to_string(),
            audience: grant.audience.to_string(),
            claims: grant
                .claims
                .iter()
                .map(|granted| GrantedClaimId {
                    claim: granted.claim.to_string(),
                    write: granted.write,
                })
                .collect(),
        }
    }
}

/// One entry's metadata — no payload bytes.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EntryListing {
    /// Whose data namespace the entry lives in.
    pub issuer: String,
    /// The entry path.
    pub path: String,
    /// How many bytes the payload holds.
    pub payload_len: u64,
}

impl From<EntryInfo> for EntryListing {
    fn from(info: EntryInfo) -> Self {
        Self {
            issuer: info.issuer.to_string(),
            path: info.path.as_str().to_owned(),
            payload_len: info.payload_len,
        }
    }
}

/// An identity from the text a screen holds.
pub(crate) fn identity(raw: &str, what: &str) -> Result<PdnId, PdnError> {
    let preview: String = raw.chars().take(64).collect();
    raw.parse()
        .map_err(|err| PdnError::malformed(format!("malformed {what} {preview:?}: {err}")))
}

/// An entry path in the runtime's own form. A path the runtime rejects is
/// reported as malformed rather than corrected.
pub(crate) fn entry_path(raw: &str) -> Result<EntryPath, PdnError> {
    let preview: String = raw.chars().take(64).collect();
    EntryPath::new(raw)
        .map_err(|err| PdnError::malformed(format!("malformed entry path {preview:?}: {err}")))
}

/// A caller's duration, refusing the two values that would otherwise
/// produce a confusing no-op or a panic downstream instead of a clean
/// refusal: zero, and anything past [`MAX_DURATION_SECS`].
pub(crate) fn duration(secs: u64, what: &str) -> Result<Duration, PdnError> {
    if secs == 0 {
        return Err(PdnError::malformed(format!("a {what} of 0 seconds")));
    }
    if secs > MAX_DURATION_SECS {
        return Err(PdnError::malformed(format!(
            "a {what} must be at most {MAX_DURATION_SECS} seconds, got {secs}"
        )));
    }
    Ok(Duration::from_secs(secs))
}

/// An optional lifetime: absent leaves the runtime's own default.
pub(crate) fn lifetime(secs: Option<u64>) -> Result<Option<Duration>, PdnError> {
    secs.map(|secs| duration(secs, "lifetime")).transpose()
}

/// The claims of a publication, derived and non-empty. A grant naming no
/// claim at all is the host's refusal: every grant is claim-scoped, and a
/// grant of nothing would publish a record that opens nothing.
pub(crate) fn granted_claims(
    issuer: PdnId,
    claims: &[GrantedPath],
) -> Result<NonEmpty<GrantedClaim>, PdnError> {
    let derived = claims
        .iter()
        .map(|granted| {
            entry_path(&granted.path).map(|path| GrantedClaim {
                claim: claim_id_of(&issuer, &path),
                write: granted.write,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    NonEmpty::from_vec(derived).ok_or_else(|| PdnError::malformed("a grant naming no claim at all"))
}

/// An entry payload on its way in: bytes, unchanged, bounded.
pub(crate) fn entry_payload(payload: &[u8]) -> Result<(), PdnError> {
    if payload.is_empty() {
        return Err(PdnError::malformed(
            "an empty entry payload, which the engine keeps no entry for",
        ));
    }
    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    if len > MAX_ENTRY_PAYLOAD {
        return Err(PdnError::malformed(format!(
            "an entry payload of {len} bytes, above the ceiling of {MAX_ENTRY_PAYLOAD}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: PdnId = PdnId::from_bytes([0x11; 32]);

    #[test]
    fn a_malformed_identity_is_the_hosts_refusal() {
        let err = identity("not-hex", "identity").unwrap_err();
        assert!(matches!(err, PdnError::MalformedInput { .. }));
    }

    #[test]
    fn a_malformed_path_is_the_hosts_refusal() {
        let err = entry_path("contact//email").unwrap_err();
        assert!(matches!(err, PdnError::MalformedInput { .. }));
    }

    /// The boundaries `duration` exists for: `0` and one past the ceiling
    /// are refusals here rather than a panic or an already-expired invite
    /// deeper in.
    #[test]
    fn a_duration_outside_its_range_is_refused() {
        assert!(duration(0, "lifetime").is_err());
        assert!(duration(MAX_DURATION_SECS, "lifetime").is_ok());
        assert!(duration(MAX_DURATION_SECS + 1, "lifetime").is_err());
        assert!(lifetime(None).unwrap().is_none());
    }

    #[test]
    fn a_grant_naming_no_claim_is_refused() {
        let err = granted_claims(ISSUER, &[]).unwrap_err();
        assert!(matches!(err, PdnError::MalformedInput { .. }));
    }

    /// The derivation a caller joins a grant read against is the runtime's
    /// own, not a second one of the facade's.
    #[test]
    fn a_publications_claim_is_the_runtimes_derivation() {
        let claims = granted_claims(
            ISSUER,
            &[GrantedPath {
                path: "contact/email".to_owned(),
                write: false,
            }],
        )
        .unwrap();
        let path = EntryPath::new("contact/email").unwrap();
        assert_eq!(claims.first().claim, claim_id_of(&ISSUER, &path));
    }

    #[test]
    fn an_empty_payload_and_one_above_the_ceiling_are_refused() {
        assert!(entry_payload(b"").is_err());
        assert!(entry_payload(b"a").is_ok());
        let too_big = vec![0_u8; usize::try_from(MAX_ENTRY_PAYLOAD).unwrap_or(usize::MAX) + 1];
        assert!(entry_payload(&too_big).is_err());
    }
}
