//! The embeddable node runtime: identity, connections, data, and sync
//! services as thin glue over `data-layer`.
//!
//! Each [`Runtime`] is one running node — a host embeds one, in-process
//! tests embed several to stand up several nodes. One runtime hosts any
//! number of identities, each added by an explicit act ([`create`] or
//! [`link`]).
//!
//! The runtime adds no sync or authorization mechanics of its own: every
//! operation delegates to a `data-layer` primitive, and access to a replica
//! remains bounded by possession of its ticket — the interim posture of
//! ADR-0008 — until subset-rbsr and `UWill` land. Hosts (HTTP today, mobile
//! and wasm later) depend on this crate; the core depends on no host
//! machinery.
//!
//! [`create`]: IdentityService::create
//! [`link`]: IdentityService::link

pub mod connections;
pub mod data;
pub mod identity;
pub mod runtime;
pub mod sync;

pub use connections::{ConnectionsService, RuntimeConnectionsService};
pub use data::{DataService, RuntimeDataService};
pub use identity::{IdentityService, RuntimeIdentityService};
pub use runtime::{Runtime, UnknownIdentity};
pub use sync::{RuntimeSyncService, SyncService};

// Vocabulary re-exports, so hosts depend on `pdn-node` alone.
pub use data_layer::{DocTicket, ShareMode, UnknownIssuer};
pub use pdn_types::{EntryInfo, EntryPath, NodeId, PdnId};
