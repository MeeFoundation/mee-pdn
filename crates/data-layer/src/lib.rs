//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! stays iroh-native and minimal. Both directions of a session are bounded
//! by the access book (`access`): the egress a session may reveal and the
//! ingest verdict on what it takes are decided per session (ADR-0014),
//! installed at spawn of that identity's own engine. A hosted identity owns
//! its half of the node (ADR-0013): [`SyncNode::provision_identity`] brings
//! it up and [`SyncNode::host_identity`] arms its PMS. A replica no records
//! can judge a caller against is refused, and a PMS keeps the ticket bound
//! Invariant 1 gives it. Errors are `anyhow`.

mod access;
mod address_book;
mod identity_key;
pub mod node;
mod path_selector;
mod pod;
pub mod private_metadata;
mod registry;

pub use access::identity_of;
#[cfg(feature = "test-util")]
pub use access::{SessionPause, SessionTrace, Traced};
pub use identity_key::{
    creates, devices_verify, join_verifies, pdn_id_of, pod_id_of, IdentityKeyPair,
};
#[cfg(feature = "test-util")]
pub use node::{
    BlobStreamHold, CollectionPause, OrderedEvents, PodObservation, PodSession, PodSessions,
    PodStartPause, StopStep, StopSteps,
};
// The ordered subscription's vocabulary, for the runtime's observation.
#[cfg(feature = "test-util")]
pub use pdn_store::{
    engine::{LiveEvent, Origin, SyncEvent},
    ContentStatus,
};
#[cfg(feature = "test-util")]
pub use pod::PodVerdicts;
pub use pod::{
    record_prefix, Awaiting, CreatedPayload, DevicesPayload, EventKind, ForNothing, HeldEntry,
    HeldPayload, JoinedPayload, Member, MemberDevice, MemberState, MembershipKey, MembershipView,
    OpId, Operation, PodCatchUp, PodHolding, PodNotice, PodStore, PodTickets, RecordEntry,
    RecordKey, RecordView, Seq, UnknownEntry, UnknownPod, Verdict, ACT_PAYLOAD,
};
// The ceremony registration point (ADR-0012, ADR-0015), re-exported so
// consumers need no direct iroh dependency. The raw `Endpoint` is
// deliberately not re-exported — the dial handle wraps it.
/// What an Android host hands the endpoint's DNS reader before a spawn, so it
/// reads the active network's resolvers instead of falling back.
#[cfg(target_os = "android")]
pub use iroh::dns::install_android_jni_context;
pub use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler},
    EndpointAddr, EndpointId,
};
pub use node::{
    AlpnTaken, Connectivity, DialHandle, DirectoryHeld, ExtraProtocol, IdentityNotProvisioned,
    PodFlush, PodPassDraw, RecordedHosting, SpawnOptions, StorageConfig, SyncNode,
    UntrackedNamespace, BUILT_IN_ALPNS, DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
};
// pdn-store vocabulary of the share and import flows, so downstream crates
// need no direct dependency on the fork.
pub use pdn_store::{api::AddrInfoOptions, AuthorId, Contact, DocTicket, Identity, NamespaceId};
pub use private_metadata::{
    pod_inviter_ticket_kind, pod_ticket_kind, CatchUpTimeout, CatchUpWatch, PrivateMetadataStore,
};
