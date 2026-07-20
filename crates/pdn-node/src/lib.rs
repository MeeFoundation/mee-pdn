//! The embeddable node runtime: identity, connections, data, and sync
//! services as thin glue over `data-layer`.
//!
//! Each [`Runtime`] is one running node — a host embeds one, in-process
//! tests embed several to stand up several nodes. One runtime hosts any
//! number of identities, each added by an explicit act ([`create`] or
//! [`link`]). Devices join an identity by the linking dialogue
//! ([`linking`], ADR-0012), and identities become connected by the
//! establishment dialogue ([`pairing`], ADR-0011) — the runtime's two
//! protocols, riding the data-layer assembly slot on the node's endpoint.
//!
//! The runtime adds no sync or authorization mechanics of its own: every
//! store operation delegates to a `data-layer` primitive, and session
//! classification lives in data-layer's access book — the runtime
//! registers what it hosts. Hosts depend on this crate; the core depends
//! on no host machinery.
//!
//! [`create`]: IdentityService::create
//! [`link`]: IdentityService::link

pub mod connections;
pub mod data;
pub mod identity;
pub mod linking;
pub mod pairing;
pub mod runtime;
pub mod sync;

pub use connections::{
    ConnectionsService, DelegationUnsupported, PeerGrant, RuntimeConnectionsService,
    ScopedPeerGrant,
};
pub use data::{DataService, RuntimeDataService};
pub use identity::{IdentityService, RuntimeIdentityService};
pub use linking::{LinkingPayload, UnsupportedLinkingVersion, LINKING_FORMAT_VERSION};
pub use pairing::{InvitePayload, UnsupportedInviteVersion, INVITE_FORMAT_VERSION};
pub use runtime::{Runtime, UnknownIdentity};
pub use sync::{RuntimeSyncService, SyncService};

// Vocabulary re-exports, so hosts depend on `pdn-node` alone.
pub use data_layer::{claim_id_of, DocTicket, ReadGrant, ShareMode, SpawnOptions, UnknownIssuer};
pub use pdn_types::{ClaimId, EntryInfo, EntryPath, NodeId, NonEmpty, PdnId};
