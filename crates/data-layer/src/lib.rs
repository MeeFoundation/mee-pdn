//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! itself stays iroh-native and minimal. The fork's ingest filter (the
//! `validate_entry` hook, ADR-0008) is not installed. Every reconciliation
//! session is classified by the access book (`access`, internal) — full
//! view for a replica identity's own devices, a capability-filtered view
//! for granted counterparties and for the devices of a grant's audience
//! identity, a refusal indistinguishable from not-hosted for everyone
//! else. Enforcement arms per identity by registration
//! ([`SyncNode::host_identity`] / [`SyncNode::host_connection`]);
//! an assembly that registers nothing is bounded by ticket possession
//! alone. One node hosts the store sets of any number of identities.
//! This crate owns:
//!
//! - [`layer`] — the entries-only [`DataLayer`] trait the node runtime
//!   drives;
//! - [`private_metadata`] — the device-replicated [`PrivateMetadataStore`]:
//!   the one directory of an identity's own state — its devices, the tickets
//!   to its other stores, and its connections;
//! - [`connection_metadata`] — the cross-identity
//!   [`ConnectionMetadataStore`]: one replica per direction of a connection,
//!   written by the issuing identity's devices, read whole by the
//!   counterparty's (Invariant 3), carrying grants;
//! - `registry` (internal) — the issuer-to-doc map data-namespace reads and
//!   writes resolve through;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + docs,
//!   addressed by issuer [`pdn_types::PdnId`] and [`pdn_types::EntryPath`]s,
//!   hosting externally supplied protocols at spawn (ADR-0011 / ADR-0012)
//!   with a narrow dial handle for their dial sides.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: at this
//! level tokens are opaque payloads.
//!
//! Errors are `anyhow`.

mod access;
pub mod connection_metadata;
pub mod grant;
pub mod layer;
pub mod node;
pub mod private_metadata;
mod registry;

pub use connection_metadata::{
    own_ticket_kind, peer_ticket_kind, ConnectionMetadata, ConnectionMetadataStore,
};
pub use grant::{claim_id_of, ReadGrant};
pub use layer::{DataLayer, DataLayerError};
pub use node::{
    AlpnTaken, DialHandle, ExtraProtocol, NamespaceImport, SpawnOptions, SyncNode, UnknownIssuer,
    BUILT_IN_ALPNS,
};
pub use private_metadata::{CatchUpTimeout, PrivateMetadataStore};

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket, NamespaceId,
};

// The ceremony registration point (ADR-0011, ADR-0012): pdn-node's pairing
// and linking handlers are written against these and registered via
// `SyncNode::spawn_with_protocols`. Re-exported so consumers need no direct
// iroh dependency and the iroh version stays pinned in one place. The raw
// `Endpoint` is deliberately not re-exported — the dial handle wraps it.
pub use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler},
    EndpointAddr, EndpointId,
};
