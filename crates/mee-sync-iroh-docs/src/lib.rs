//! Data-layer adapter: capability-gated document sync over the iroh-docs
//! variant.
//!
//! This crate is the middle layer between the PDN domain and our forked
//! `iroh-docs`. The fork stays iroh-native and minimal — its only seam is
//! the `CapabilityValidator` hook (`Fn(&SignedEntry) -> bool`) consulted at
//! the `validate_entry` chokepoint. Everything Mee-flavoured lives here:
//!
//! - [`gate`] — the domain-level [`IngestPolicy`] trait, its bridge into the
//!   fork's hook, and the naive connections-based policy — the single-link
//!   precursor of full UWill chain validation;
//! - `registry` (internal) — binding of domain [`mee_sync_api::NamespaceId`]
//!   (the `(about, issued_by)` pair) to the iroh docs that back it, shared
//!   with the gate so incoming entries resolve to domain terms;
//! - [`node`] — the assembled stack: endpoint + gossip + blobs + gated docs,
//!   addressed by domain namespace ids and [`mee_sync_api::EntryPath`]s.
//!
//! Errors are `anyhow` for now; typed errors arrive together with the
//! `WillowLayer` trait implementation.

pub mod gate;
pub mod node;
mod registry;

pub use gate::{Admission, Connections, ConnectionsPolicy, IngestCtx, IngestPolicy};
pub use node::SyncNode;

// Re-exported iroh-docs vocabulary for the common share/import/write flows,
// so downstream crates don't need a direct iroh-docs dependency.
pub use iroh_docs::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};
