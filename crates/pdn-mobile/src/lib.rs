//! The mobile host: a uniffi facade over the [`pdn_node`] runtime, the
//! second host of the same shape as `pdn-node-http` — one exported call per
//! service call of the runtime and no orchestration of its own, since an
//! application and a facade each keeping a view of the same thing would
//! drift. What stays absent from it, why its error table names more kinds
//! than the other host's, and the `observation` build are in the crate's
//! `CLAUDE.md`; its requirements are the `pdn-mobile/host` spec.

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
