//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! stays iroh-native and minimal. Both directions of a session are bounded
//! by the access book (`access`): reads are classified per session and
//! filtered on egress, writes are judged per entry by the fork's ingest
//! hook (ADR-0008), both installed at spawn of that identity's own
//! engine. A hosted identity owns its half of the node (ADR-0013):
//! [`SyncNode::provision_identity`] brings it up, [`SyncNode::host_identity`]
//! arms its PMS and [`SyncNode::host_connection`] its connections.
//! A data replica no records can judge a caller against is refused, and a
//! PMS and a connection metadata store keep the ticket bound
//! Invariants 1 and 3 give them.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: tokens
//! are opaque payloads at this level. Errors are `anyhow`.

mod access;
mod address_book;
mod announcement;
pub mod connection_metadata;
pub mod grant;
pub mod layer;
pub mod node;
mod pod;
pub mod private_metadata;
mod registry;
mod retraction;

pub use access::identity_of;
pub use announcement::{devices_verify, join_verifies, pdn_id_of, pod_id_of, AnnouncementKeyPair};
pub use connection_metadata::{
    own_ticket_kind, peer_ticket_kind, ConnectionMetadata, ConnectionMetadataStore, GrantRead,
};
pub use grant::{claim_id_of, GrantedClaim, ReadGrant};
#[cfg(feature = "test-util")]
pub use node::{PodSession, PodSessions, PodStartPause};
#[cfg(feature = "test-util")]
pub use pod::PodVerdicts;
pub use pod::{
    record_prefix, Awaiting, CreatedPayload, DevicesPayload, EventKind, ForNothing, HeldEntry,
    JoinedPayload, Member, MemberDevice, MemberState, Membership, MembershipKey, OpId, Operation,
    PodCatchUp, PodNotice, PodStore, PodTickets, RecordEntry, RecordKey, RecordView, Seq,
    UnknownEntry, UnknownPod, Verdict, ACT_PAYLOAD,
};
// The ceremony registration point (ADR-0011, ADR-0012), re-exported so
// consumers need no direct iroh dependency. The raw `Endpoint` is
// deliberately not re-exported — the dial handle wraps it.
pub use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler},
    EndpointAddr, EndpointId,
};
pub use layer::{DataLayer, DataLayerError};
pub use node::{
    AlpnTaken, Connectivity, DialHandle, DirectoryHeld, ExtraProtocol, GranteeCannotShare,
    IdentityNotProvisioned, NamespaceImport, PodFlush, PodPassDraw, RecordedHosting, SpawnOptions,
    StorageConfig, SyncNode, UnknownIssuer, UntrackedNamespace, BUILT_IN_ALPNS,
    DEFAULT_REPLICA_CACHE_BUDGET_BYTES,
};
// pdn-store vocabulary of the share/import/write flows, so downstream
// crates need no direct dependency on the fork.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, Contact, DocTicket, Identity, NamespaceId,
};
pub use private_metadata::{
    pod_inviter_ticket_kind, pod_ticket_kind, CatchUpTimeout, CatchUpWatch, PrivateMetadataStore,
    RetractionHead, RetractionMarker,
};
pub use retraction::RetractionVerdict;
