//! The data layer: capability-gated document sync over our iroh-docs
//! variant.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! itself stays iroh-native and minimal — its only seam is the
//! `CapabilityValidator` hook (`Fn(&SignedEntry) -> bool`) consulted at
//! the `validate_entry` chokepoint. This crate owns:
//!
//! - [`layer`] — the entries-only [`DataLayer`] trait the node runtime
//!   drives; this crate is where its implementation lives;
//! - [`gate`] — the domain-level [`IngestPolicy`] trait, its bridge into the
//!   fork's hook, the device axiom ([`SelfOwned`]) and the naive
//!   connections-based data policy — single-link precursors of full `UWill`
//!   chain validation;
//! - [`connections`] — the device-replicated [`ConnectionsStore`]: an
//!   identity's connections as a dedicated replica its devices converge on;
//! - `registry` (internal) — binding of an iroh replica to its domain
//!   [`Binding`] (a data [`pdn_types::NamespaceId`] or the connections store),
//!   shared with the gate so incoming entries resolve to domain terms;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + gated docs,
//!   addressed by domain namespace ids and [`pdn_types::EntryPath`]s.
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
pub mod node;
mod registry;

pub use connections::ConnectionsStore;
pub use gate::{
    Admission, AnyOf, Connections, ConnectionsPolicy, IngestCtx, IngestPolicy, SelfOwned,
};
pub use layer::{DataLayer, DataLayerError};
pub use node::SyncNode;
pub use registry::Binding;

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};
