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
//! - [`connections`] — the device-replicated [`ConnectionsStore`]: an
//!   identity's connections as a dedicated replica replicated across its devices;
//! - [`private_metadata`] — the device-replicated [`PrivateMetadataStore`]:
//!   an identity's devices and the tickets to its other stores (the bootstrap
//!   directory a newly linked device reads from);
//! - [`linking`] — [`provision_identity`] / [`link_device`]: bring an
//!   identity up on its first device, and every further device up from a
//!   single seed (the private-metadata-store ticket), bootstrapping the rest
//!   through that directory; run once per identity to host several;
//! - `registry` (internal) — the issuer-to-doc map data-namespace reads and
//!   writes resolve through;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + docs,
//!   addressed by issuer [`pdn_types::PdnId`] and [`pdn_types::EntryPath`]s,
//!   hosting ADR-0011's pairing protocol at spawn with a narrow dial handle
//!   onto the endpoint exposed for its dial side.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: at this
//! level tokens are opaque payloads.
//!
//! Errors are `anyhow` for now; typed errors arrive together with the
//! [`DataLayer`] implementation.

pub mod connections;
pub mod layer;
pub mod linking;
pub mod node;
pub mod private_metadata;
mod registry;

pub use connections::ConnectionsStore;
pub use layer::{DataLayer, DataLayerError};
pub use linking::{link_device, provision_identity, IdentityStores};
pub use node::{AlpnTaken, DialHandle, ExtraProtocol, SyncNode, UnknownIssuer, BUILT_IN_ALPNS};
pub use private_metadata::PrivateMetadataStore;

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};

// The pairing registration point (ADR-0011): implement the pairing handler against these
// and register it via `SyncNode::spawn_with_protocols`; reach the dial side
// through `SyncNode::dial_handle`. Re-exported so consumers (the pdn-node
// runtime) need no direct iroh dependency and the iroh version stays pinned
// in one place. The raw `Endpoint` is deliberately not re-exported — a
// consumer never handles one; the dial handle wraps it.
pub use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, DynProtocolHandler, ProtocolHandler},
    EndpointAddr, EndpointId,
};
