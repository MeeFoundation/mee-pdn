//! The mobile host: a uniffi facade over the [`pdn_node`] runtime, and the
//! second host of the same shape as `pdn-node-http`.
//!
//! One exported call per service call of the runtime, and no orchestration
//! of the facade's own: it retries no ceremony, remembers no result, and
//! holds no rule about what a member may read. An application
//! and a facade that each kept a view of the same thing would drift, and
//! the drift would show as a screen contradicting the node it sits on.
//!
//! # What is deliberately absent
//!
//! No exported call hands over or accepts a namespace ticket. A pod's
//! tickets cross only inside the join dialogue, so an application able to
//! import a ticket would hold a pod's stores the membership never admitted
//! it to.
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
//! unexported. The one exception is the sync tests' build behind the
//! `observation` feature, which `pdn-sdk` never packages as a release.
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
//! material behind it, so nothing here proves who a peer is. A pod's
//! membership is evidence that a device ran a join with the same one-time
//! secret, and nothing more.
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
#[cfg(feature = "observation")]
mod test_surface;

pub use error::{PdnError, PodActRefusal};
pub use node::PdnNode;
pub use shapes::{
    PodMemberInfo, PodMembershipAct, PodOperation, PodRecord, PodRecordKind, MAX_ENTRY_PAYLOAD,
};

uniffi::setup_scaffolding!();

/// The ceiling on one record payload, in bytes — the same value
/// [`MAX_ENTRY_PAYLOAD`] states, reachable from a screen that wants to
/// refuse before it calls.
#[uniffi::export]
pub fn max_entry_payload() -> u64 {
    MAX_ENTRY_PAYLOAD
}
