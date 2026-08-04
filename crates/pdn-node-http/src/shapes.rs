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

/// The budget of a whole `link` act — its dialogue and its catch-up — when
/// the caller names none.
const DEFAULT_LINK_BUDGET: Duration = Duration::from_secs(30);

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
    pub fn as_duration(&self) -> Option<Duration> {
        self.lifetime_secs.map(Duration::from_secs)
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
    pub fn as_duration(&self) -> Duration {
        self.timeout_secs
            .map_or(DEFAULT_LINK_BUDGET, Duration::from_secs)
    }
}

/// A grant publication: whose data, and exactly which claims with which
/// commands. An empty claim set is malformed — every grant is claim-scoped.
#[derive(Debug, Serialize, Deserialize)]
pub struct GrantPublication {
    pub issuer: PdnId,
    pub claims: Vec<GrantedClaim>,
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
    pub grants: Vec<ReadGrant>,
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
