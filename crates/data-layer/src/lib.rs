//! The data layer: capability-gated document sync over pdn-store, our
//! iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! itself stays iroh-native and minimal — its only seam is the
//! `CapabilityValidator` hook (`Fn(&SignedEntry) -> bool`) consulted at
//! the `validate_entry` chokepoint. This crate owns:
//!
//! - [`layer`] — the entries-only [`DataLayer`] trait the node runtime
//!   drives; this crate is where its implementation lives;
//! - [`gate`] — the domain-level [`IngestPolicy`] trait, its bridge into the
//!   fork's hook, [`SelfOwned`] (a node admits its own identity's replicas)
//!   and the naive connections-based data policy — a single-link precursor of
//!   full `UWill` chain validation;
//! - [`connections`] — the device-replicated [`ConnectionsStore`]: an
//!   identity's connections as a dedicated replica replicated across its devices;
//! - [`private_metadata`] — the device-replicated [`PrivateMetadataStore`]:
//!   an identity's devices and the tickets to its other stores (the bootstrap
//!   directory a newly linked device reads from);
//! - [`linking`] — [`link_device`]: bring a new device up from a single seed
//!   (the private-metadata-store ticket), bootstrapping the rest through that
//!   directory;
//! - `registry` (internal) — binding of an iroh replica to its domain
//!   [`Binding`] (a data namespace keyed by issuer, or a device store),
//!   shared with the gate so incoming entries resolve to domain terms;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + gated docs,
//!   addressed by issuer [`pdn_types::PdnId`] and [`pdn_types::EntryPath`]s.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: at this
//! level tokens are opaque payloads, and ingest-time checks arrive as
//! injected [`IngestPolicy`] objects, constructed above.
//!
//! Errors are `anyhow` for now; typed errors arrive together with the
//! [`DataLayer`] implementation.

pub mod connections;
pub mod gate;
pub mod layer;
pub mod linking;
pub mod node;
pub mod private_metadata;
mod registry;

pub use connections::ConnectionsStore;
pub use gate::{
    Admission, AnyOf, Connections, ConnectionsPolicy, IngestCtx, IngestPolicy, SelfOwned,
};
pub use layer::{DataLayer, DataLayerError};
pub use linking::{link_device, LinkedStores};
pub use node::SyncNode;
pub use private_metadata::PrivateMetadataStore;
pub use registry::Binding;

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};
