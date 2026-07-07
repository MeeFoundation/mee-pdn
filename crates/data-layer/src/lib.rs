//! The data layer: document sync over pdn-store, our iroh-docs fork.
//!
//! Everything platform-specific around the fork lives here, so the fork
//! itself stays iroh-native and minimal. Its ingest seam — the
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
//!   addressed by issuer [`pdn_types::PdnId`] and [`pdn_types::EntryPath`]s.
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
pub use node::{SyncNode, UnknownIssuer};
pub use private_metadata::PrivateMetadataStore;

// Re-exported pdn-store (iroh-docs fork) vocabulary for the common
// share/import/write flows, so downstream crates don't need a direct
// dependency on it.
pub use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    AuthorId, DocTicket,
};
