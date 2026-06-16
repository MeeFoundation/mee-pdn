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
//!   fork's hook, and the naive connections-based policy — the single-link
//!   precursor of full `UWill` chain validation;
//! - `registry` (internal) — binding of domain [`pdn_types::NamespaceId`]
//!   (the `(about, issued_by)` pair) to the iroh docs that back it, shared
//!   with the gate so incoming entries resolve to domain terms;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + gated docs,
//!   addressed by domain namespace ids and [`pdn_types::EntryPath`]s.
//!
//! Capability *semantics* (`UWill` tokens, chains) do not live here: at this
//! level tokens are opaque payloads, and ingest-time checks arrive as
//! injected [`IngestPolicy`] objects, constructed above.
//!
//! Errors are `anyhow` for now; typed errors arrive together with the
//! [`DataLayer`] implementation.

pub mod gate;
pub mod layer;
pub mod node;
mod registry;

pub use gate::{Admission, Connections, ConnectionsPolicy, IngestCtx, IngestPolicy};
pub use layer::{DataLayer, DataLayerError};
pub use node::SyncNode;

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};
