//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! itself stays iroh-native and minimal. Its ingest filter — the
//! `CapabilityValidator` hook (`Fn(&SignedEntry) -> bool`) consulted at the
//! `validate_entry` chokepoint (ADR-0008) — remains in the fork but is not
//! installed: until subset-rbsr (egress filtering) and `UWill` land, access
//! to a replica is bounded by possession of its ticket and by nothing else.
//! One node hosts the store sets of any number of identities side by side.
//! This crate owns:
//!
//! - [`layer`] — the entries-only [`DataLayer`] trait the node runtime
//!   drives; this crate is where its implementation lives;
//! - [`private_metadata`] — the device-replicated [`PrivateMetadataStore`]:
//!   the one directory of an identity's own state — its devices, the tickets
//!   to its other stores, and its connections;
//! - [`connection_metadata`] — the cross-identity
//!   [`ConnectionMetadataStore`]: one replica per direction of a connection,
//!   written by the issuing identity's devices, read whole by the
//!   counterparty's (Invariant 3), carrying the grants everything later
//!   rides on;
//! - `registry` (internal) — the issuer-to-doc map data-namespace reads and
//!   writes resolve through;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + docs,
//!   addressed by issuer [`pdn_types::PdnId`] and [`pdn_types::EntryPath`]s,
//!   hosting externally supplied protocols at spawn (pdn-node's pairing and
//!   linking dialogues, ADR-0011 / ADR-0012) with a narrow dial handle onto
//!   the endpoint exposed for their dial sides.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: at this
//! level tokens are opaque payloads.
//!
//! Errors are `anyhow` for now; typed errors arrive together with the
//! [`DataLayer`] implementation.

pub mod connection_metadata;
pub mod layer;
pub mod node;
pub mod private_metadata;
mod registry;

pub use connection_metadata::{
    own_ticket_kind, peer_ticket_kind, ConnectionMetadata, ConnectionMetadataStore,
};
pub use layer::{DataLayer, DataLayerError};
pub use node::{AlpnTaken, DialHandle, ExtraProtocol, SyncNode, UnknownIssuer, BUILT_IN_ALPNS};
pub use private_metadata::{CatchUpTimeout, PrivateMetadataStore};

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket, NamespaceId,
};

// The ceremony registration point (ADR-0011, ADR-0012): the pdn-node
// runtime's pairing and linking handlers are written against these and
// registered via `SyncNode::spawn_with_protocols`; their dial sides reach
// the endpoint through `SyncNode::dial_handle`. Re-exported so consumers
// (the pdn-node runtime) need no direct iroh dependency and the iroh
// version stays pinned in one place. The raw `Endpoint` is deliberately not
// re-exported — a consumer never handles one; the dial handle wraps it.
pub use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler},
    EndpointAddr, EndpointId,
};
