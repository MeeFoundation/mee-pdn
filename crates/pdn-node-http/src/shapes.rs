//! The request and response bodies of the debug surface.
//!
//! Structured values are JSON built from types the runtime already
//! serializes; entry payloads are not here at all — they travel as raw
//! request and response bodies, so nothing sits between a written byte
//! string and a read one.
//!
//! The ceremony payloads are absent too, for the opposite reason: the host
//! passes them through as whole values and never names their fields, the
//! way a person carries a code from one screen to another.
//!
//! Every shape round-trips, so a caller in this repository builds what it
//! sends and reads what it gets from the same definitions. They are
//! scaffolding all the same — unpinned, and no contract for anything
//! outside.

use std::time::Duration;

use pdn_node::{EntryInfo, GrantedClaim, PdnId, ReadGrant};
use serde::{Deserialize, Serialize};

use crate::error::HostError;

/// The budget of a whole `link` act — its dialogue and its catch-up — when
/// the caller names none.
const DEFAULT_LINK_BUDGET: Duration = Duration::from_secs(30);

/// The widest duration a caller may name for a lifetime or a budget —
/// generous, and nowhere near the point where adding it to the runtime's
/// current instant would overflow.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

/// A caller-supplied duration in seconds, rejecting the two values that
/// would otherwise produce a confusing no-op or a downstream panic instead
/// of a clean refusal: zero (an already-expired invite, a budget that times
/// out immediately) and anything past [`MAX_DURATION_SECS`] (which would
/// overflow `Instant + Duration` in the runtime rather than fail here).
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

/// The identity a create call minted.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreatedIdentity {
    pub identity: PdnId,
}

/// The identities this runtime hosts.
#[derive(Debug, Serialize, Deserialize)]
pub struct HostedIdentities {
    pub identities: Vec<PdnId>,
}

/// A hosted identity's current connections.
#[derive(Debug, Serialize, Deserialize)]
pub struct Connections {
    pub connections: Vec<PdnId>,
}

/// How long a minted invite lives, in seconds; unset leaves the runtime's
/// own short default.
///
/// Unknown parameters are refused rather than ignored: a mistyped
/// `lifetime_secs` that fell back to the default would mint an invite with
/// a lifetime nobody asked for, and nothing in the answer would say so.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifetime {
    pub lifetime_secs: Option<u64>,
}

impl Lifetime {
    /// The lifetime as the service takes it: `None` means "your default".
    /// Rejects a caller-supplied `0` or a value past [`MAX_DURATION_SECS`].
    pub fn as_duration(&self) -> Result<Option<Duration>, HostError> {
        self.lifetime_secs.map(duration_in_range).transpose()
    }
}

/// The budget of a `link` act, in seconds. A mistyped parameter is refused
/// like any other malformed request, for the reason [`Lifetime`] states.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkBudget {
    pub timeout_secs: Option<u64>,
}

impl LinkBudget {
    /// The budget as the service takes it — the caller's, or the default.
    /// Rejects a caller-supplied `0` or a value past [`MAX_DURATION_SECS`].
    pub fn as_duration(&self) -> Result<Duration, HostError> {
        self.timeout_secs
            .map_or(Ok(DEFAULT_LINK_BUDGET), duration_in_range)
    }
}

/// A grant publication: whose data, and exactly which claims with which
/// commands. An empty claim set is malformed — every grant is claim-scoped.
/// A mistyped field is refused rather than silently ignored, for the reason
/// [`Lifetime`] states.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPublication {
    pub issuer: PdnId,
    pub claims: Vec<GrantedPath>,
}

/// One claim of a publication, named by the path it covers.
///
/// A claim's identity is arithmetic on the issuer and the path, and the host
/// does it: every other route of this surface addresses an entry by path, so
/// a caller that had to derive the identity itself would be reproducing a
/// rule of the product to talk to it. What crosses is what a person can
/// write down.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedPath {
    pub path: String,
    pub write: bool,
}

/// One grant's capability, without its ticket — an HTTP-owned shape,
/// deliberately distinct from [`ReadGrant`] (the durable, internal record
/// type): `deny_unknown_fields` here rejects a field the surface never
/// promised, ticket included, whatever it is named — a property `ReadGrant`
/// itself must not carry, since it is also the wire format the store
/// replicates and a future internal field there must not become a hard
/// deserialization error for every past record.
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

/// The grants a peer published toward a hosted identity — the capability
/// alone.
///
/// The ticket each grant carries stays behind. The runtime binds a granted
/// namespace by itself as the grant record replicates, so no caller needs
/// the ticket to make a grant work; handing it over would let a test
/// arrange the granted namespace by importing it, and such a test keeps
/// passing after the grant binder breaks.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerGrants {
    pub grants: Vec<GrantCapability>,
}

/// The grant a hosted identity published toward a peer, read on the device
/// that answers — the capability alone, its ticket left behind for the
/// reason [`PeerGrants`] states. One at most: the identity's own half of
/// the pair holds one record, its whole claim set inside the capability.
///
/// The answer is this device's. It says the record is readable here, never
/// that it reached a sibling or the peer, and an absent one covers a device
/// with no connection toward the peer, a pair whose tickets have not
/// replicated here, a record here whose payload cannot be read yet, and
/// nothing granted alike.
#[derive(Debug, Serialize, Deserialize)]
pub struct OwnGrant {
    pub grant: Option<GrantCapability>,
}

/// Entry metadata under one issuer — no payload bytes.
#[derive(Debug, Serialize, Deserialize)]
pub struct Entries {
    pub entries: Vec<EntryInfo>,
}

/// The listing's optional path prefix, matching whole components. A
/// mistyped parameter is refused like any other malformed request, for the
/// reason [`Lifetime`] states — here it would silently widen a listing to
/// the whole namespace.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingPrefix {
    pub prefix: Option<String>,
}

/// No query parameters at all — for a route with none of its own, so an
/// unknown parameter is refused here exactly as it would be on a route that
/// does take one, for the reason [`Lifetime`] states.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoQuery {}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    /// The boundary `duration_in_range` exists to enforce: `0` is a request
    /// error, not a downstream panic; `MAX_DURATION_SECS` is the last value
    /// still accepted; one past it refuses the same way `0` does. A
    /// regression here (the `0` check dropped, the ceiling raised, the
    /// comparison flipped) would not fail any existing HTTP-level test —
    /// this is the only place these three values are asserted directly.
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
