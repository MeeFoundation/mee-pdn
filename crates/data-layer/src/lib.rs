//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! stays iroh-native and minimal. Both directions of a session are bounded
//! by the access book (`access`): reads are classified per session and
//! filtered on egress, writes are judged per entry by the fork's ingest
//! hook (ADR-0008), both installed at spawn. Enforcement arms per identity
//! by registration ([`SyncNode::host_identity`] /
//! [`SyncNode::host_connection`]); an assembly that registers nothing is
//! bounded by ticket possession alone.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: tokens
//! are opaque payloads at this level. Errors are `anyhow`.

mod access;
pub mod connection_metadata;
pub mod grant;
pub mod layer;
pub mod node;
pub mod private_metadata;
mod registry;
mod retraction;

pub use connection_metadata::{
    own_ticket_kind, peer_ticket_kind, ConnectionMetadata, ConnectionMetadataStore, GrantRead,
};
pub use grant::{claim_id_of, GrantedClaim, ReadGrant};
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
    AlpnTaken, Connectivity, DialHandle, DirectoryHeld, ExtraProtocol, NamespaceImport,
    SpawnOptions, StorageConfig, SyncNode, UnknownIssuer, UntrackedNamespace, BUILT_IN_ALPNS,
};
// pdn-store vocabulary of the share/import/write flows, so downstream
// crates need no direct dependency on the fork.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket, NamespaceId,
};
pub use private_metadata::{CatchUpTimeout, PrivateMetadataStore, RetractionMarker};
pub use retraction::RetractionVerdict;
