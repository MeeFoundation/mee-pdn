//! The request and response bodies of the debug surface: JSON built from
//! types the runtime already serializes. Entry payloads travel as raw
//! bodies, and ceremony payloads pass through as whole values the host
//! never names. Scaffolding — unpinned, and no contract for anything
//! outside this repository.

use std::time::Duration;

use pdn_node::{EntryInfo, GrantedClaim, PdnId, ReadGrant};
use serde::{Deserialize, Serialize};

use crate::error::HostError;

/// The budget of a whole `link` act when the caller names none.
const DEFAULT_LINK_BUDGET: Duration = Duration::from_secs(30);

/// Nowhere near where `Instant + Duration` would overflow in the runtime.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

/// Zero would be a confusing no-op (an already-expired invite), and past
/// the ceiling a downstream panic instead of a clean refusal.
fn duration_in_range(secs: u64) -> Result<Duration, HostError> {
    if secs == 0 {
        return Err(HostError::bad_request(
            "a duration of 0 seconds is not allowed",
        ));
    }
    if secs > MAX_DURATION_SECS {
        return Err(HostError::bad_request(format!(
            "a duration must be at most {MAX_DURATION_SECS} seconds, got {secs}"
        )));
    }
    Ok(Duration::from_secs(secs))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreatedIdentity {
    pub identity: PdnId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostedIdentities {
    pub identities: Vec<PdnId>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Connections {
    pub connections: Vec<PdnId>,
}

/// Unknown parameters are refused rather than ignored: a mistyped
/// `lifetime_secs` that fell back to the default would mint an invite with
/// a lifetime nobody asked for, and nothing in the answer would say so.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifetime {
    pub lifetime_secs: Option<u64>,
}

impl Lifetime {
    /// `None` means the service's default.
    pub fn as_duration(&self) -> Result<Option<Duration>, HostError> {
        self.lifetime_secs.map(duration_in_range).transpose()
    }
}

/// See [`Lifetime`] for `deny_unknown_fields`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkBudget {
    pub timeout_secs: Option<u64>,
}

impl LinkBudget {
    pub fn as_duration(&self) -> Result<Duration, HostError> {
        self.timeout_secs
            .map_or(Ok(DEFAULT_LINK_BUDGET), duration_in_range)
    }
}

/// An empty claim set is malformed. See [`Lifetime`] for
/// `deny_unknown_fields`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPublication {
    pub issuer: PdnId,
    pub claims: Vec<GrantedPath>,
}

/// One claim, named by its path: the host derives the claim identity, so a
/// caller need not reproduce a rule of the product to talk to it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedPath {
    pub path: String,
    pub write: bool,
}

/// One grant's capability without its ticket — deliberately distinct from
/// [`ReadGrant`]: `deny_unknown_fields` here rejects a field the surface
/// never promised, ticket included, whereas `ReadGrant` is the wire format
/// the store replicates and must keep decoding past records after a field
/// is added.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantCapability {
    pub issuer: PdnId,
    pub audience: PdnId,
    pub claims: Vec<GrantedClaim>,
}

impl From<ReadGrant> for GrantCapability {
    fn from(grant: ReadGrant) -> Self {
        Self {
            issuer: grant.issuer,
            audience: grant.audience,
            claims: grant.claims.into_iter().collect(),
        }
    }
}

/// The capabilities alone: a ticket handed over here would let a test
/// arrange a granted namespace by importing it, and such a test keeps
/// passing after the grant binder breaks.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerGrants {
    pub grants: Vec<GrantCapability>,
}

/// This device's answer, capability alone: it says the record is readable
/// here, never that it reached a sibling or the peer.
#[derive(Debug, Serialize, Deserialize)]
pub struct OwnGrant {
    pub grant: Option<GrantCapability>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entries {
    pub entries: Vec<EntryInfo>,
}

/// A mistyped parameter would silently widen a listing to the whole
/// namespace.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingPrefix {
    pub prefix: Option<String>,
}

/// For a route with no parameters of its own, so an unknown one is refused
/// there too.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoQuery {}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    /// The only place the three boundary values are asserted directly.
    #[test]
    fn duration_in_range_rejects_zero_and_anything_past_the_ceiling() {
        assert!(duration_in_range(0).is_err());
        assert_eq!(
            duration_in_range(0).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        assert!(duration_in_range(MAX_DURATION_SECS).is_ok());
        assert_eq!(
            duration_in_range(MAX_DURATION_SECS).unwrap(),
            Duration::from_secs(MAX_DURATION_SECS)
        );
        assert!(duration_in_range(MAX_DURATION_SECS + 1).is_err());
        assert_eq!(
            duration_in_range(MAX_DURATION_SECS + 1)
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
