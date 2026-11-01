//! The mobile host: a uniffi facade over the [`pdn_node`] runtime, the
//! second host of the same shape as `pdn-node-http` — one exported call per
//! service call of the runtime and no orchestration of its own, since an
//! application and a facade each keeping a view of the same thing would
//! drift. What stays absent from it, why its error table names more kinds
//! than the other host's, and the `observation` build are in the crate's
//! `CLAUDE.md`; its requirements are the `pdn-mobile/host` spec.

#[cfg(target_os = "android")]
mod android;
mod error;
mod node;
mod payload;
mod shapes;
mod streams;
#[cfg(feature = "observation")]
mod test_surface;

pub use error::{PdnError, PodActRefusal};
pub use node::PdnNode;
pub use shapes::{
    InviteState, PodMemberInfo, PodMembershipAct, PodOperation, PodRecord, PodRecordKind,
    MAX_DOCUMENT_PAYLOAD, MAX_ENTRY_PAYLOAD,
};
pub use streams::{RecordReader, RecordWriter};

uniffi::setup_scaffolding!();

/// The ceiling on one payload of a record of `kind`, in bytes — [`MAX_DOCUMENT_PAYLOAD`] for an
/// immutable-document, [`MAX_ENTRY_PAYLOAD`] for a claim and for each operation of a
/// mergeable-document — reachable from a screen that wants to refuse before it calls.
#[uniffi::export]
pub fn max_payload(kind: PodRecordKind) -> u64 {
    shapes::max_payload(kind)
}
