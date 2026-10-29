//! The embeddable node runtime: identity, pods and sync services as thin
//! glue over `data-layer`, plus the runtime's two protocols — the linking
//! dialogue ([`linking`], ADR-0012) and a pod's join ([`pods`], ADR-0015) —
//! riding the data-layer assembly slot. The runtime adds no sync or
//! authorization mechanics of its own: it registers what it hosts with
//! data-layer's access book.

mod armer;
mod ceremony;
pub mod identity;
pub mod linking;
#[cfg(feature = "test-util")]
pub mod observation;
pub mod pods;
pub mod runtime;
pub mod sync;

#[cfg(feature = "test-util")]
pub use pods::ForgedJoin;
pub use pods::{
    ActRefusal, ActRefused, IdentityKeyPending, JoinInProgress, JoinRefused, JoinTimeout, PodAct,
    PodInfo, PodInvite, PodMember, PodsService, RecordPlacedOnce, RuntimePodsService,
    UnknownRecord, UnsupportedPodInviteVersion, WrongRecordKind, JOIN_CATCH_UP_TIMEOUT,
    JOIN_DIALOGUE_TIMEOUT, LEAVE_FLUSH_TIMEOUT, POD_INVITE_FORMAT_VERSION,
};
// Vocabulary re-exports, so hosts depend on `pdn-node` alone.
pub use ceremony::{InviteState, InviterUnreachable};
#[cfg(target_os = "android")]
pub use data_layer::install_android_jni_context;
pub use data_layer::{
    CatchUpTimeout, Connectivity, DirectoryHeld, OpId, Operation, PayloadReader, PayloadStream,
    PodStore, SpawnOptions, StorageConfig, UnknownEntry, UnknownPod,
};
pub use identity::{IdentityService, RuntimeIdentityService};
pub use linking::{
    DialogueTimeout, IdentityAlreadyHosted, LinkingInProgress, LinkingLocalFailure, LinkingPayload,
    LinkingRefused, UnsupportedLinkingVersion, LINKING_FORMAT_VERSION,
};
pub use pdn_types::{NodeId, PdnId, PodId, RecordId, RecordKind, RecordRef};
pub use runtime::{Runtime, UnknownIdentity};
pub use sync::{RuntimeSyncService, SyncService};
