//! The mobile host: a uniffi facade over the [`pdn_node`] runtime, and the
//! second host of the same shape as `pdn-node-http`.
//!
//! One exported call per service call of the runtime, and no orchestration
//! of the facade's own: it retries no ceremony, caches no grant, remembers
//! no result, and holds no rule about what a peer may read. An application
//! and a facade that each kept a view of the same thing would drift, and
//! the drift would show as a screen contradicting the node it sits on.
//!
//! # What is deliberately absent
//!
//! No exported call hands over or accepts a namespace ticket. A grant read
//! reports the capability alone — issuer, audience, claims, commands — and
//! the ticket the runtime's read of a peer's grant carries beside it is
//! dropped here. The sanctioned way a granted namespace arrives is that the
//! runtime binds what the grant names, so an application able to import a
//! ticket would keep showing data after that binding broke, with nothing in
//! the result revealing it.
//!
//! No exported call shares or imports a namespace, forces a
//! reconciliation, resets state, drops a replica, or reaches a store
//! outside a service operation. Repeating a read is the whole of waiting: a
//! screen given a synchronize control would keep showing convergence after
//! convergence broke.
//!
//! None of the runtime's test-only surface is reachable, and the facade is
//! built with that feature off, so a forced write and an observation of a
//! replica's contact set are absent from the binary rather than merely
//! unexported.
//!
//! The facade authorizes nothing: no credential, token or permission of its
//! own on any exported call. Reaching the facade is reaching the node, and
//! it runs inside the application's process, so the application's own
//! screen lock is the whole of the posture in front of it.
//!
//! # What it repeats from the runtime, and adds nothing to
//!
//! A node keeps its replicas, its payloads and its own key in the directory
//! named at bring-up, and comes back on that directory as the same node
//! hosting what it hosted. What a restart does not bring back is work in
//! flight — an invite minted and not consumed, a ceremony interrupted. The
//! device's storage is the only copy: nothing behind this surface keeps
//! another.
//!
//! An identity is a placeholder value the runtime mints, with no key
//! material behind it, so nothing here proves who a peer is. A connection
//! is evidence that 2 devices ran a ceremony with the same one-time secret,
//! and nothing more.
//!
//! A peer is reached only in one local network: the runtime's endpoint binds
//! with no relay and no discovery configured, so a peer is reached at an
//! address the endpoint publishes about itself.
//!
//! # Its error table diverges from the other host's, on purpose
//!
//! `pdn-node-http` folds an unreachable counterparty and every ceremony
//! timeout into its unrecognized failure, because for a container test the
//! distinction a denial rests on is a refusal against a defect. A person
//! holding a phone needs different acts out of those outcomes, so
//! [`PdnError`] names them. The divergence is stated rather than left for a
//! reader comparing the two hosts to find.

mod error;
mod node;
mod payload;
mod shapes;

pub use error::PdnError;
pub use node::PdnNode;
pub use shapes::{EntryListing, GrantCapability, GrantedClaimId, GrantedPath, MAX_ENTRY_PAYLOAD};

uniffi::setup_scaffolding!();

/// The claim identity of `path` in `issuer`'s data — the derivation a grant
/// carries, exported so a caller can join a grant read against paths it
/// knows or has listed.
///
/// The derivation is one-way: nothing recovers a path from a claim
/// identity. A screen that wants to show which fields are shared derives
/// the identities of the paths it knows and compares, and never invents a
/// name for one it cannot account for.
#[uniffi::export]
pub fn claim_id(issuer: &str, path: &str) -> Result<String, PdnError> {
    let issuer = shapes::identity(issuer, "issuer")?;
    let path = shapes::entry_path(path)?;
    Ok(pdn_node::claim_id_of(&issuer, &path).to_string())
}

/// The ceiling on one entry payload, in bytes — the same value
/// [`MAX_ENTRY_PAYLOAD`] states, reachable from a screen that wants to
/// refuse before it calls.
#[uniffi::export]
pub fn max_entry_payload() -> u64 {
    MAX_ENTRY_PAYLOAD
}
