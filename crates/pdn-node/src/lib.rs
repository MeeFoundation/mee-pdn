//! The embeddable node runtime: identity, connections, data, and sync
//! services as thin glue over `data-layer`, plus the runtime's two
//! protocols — the linking dialogue ([`linking`], ADR-0012) and the
//! establishment dialogue ([`pairing`], ADR-0011) — riding the data-layer
//! assembly slot. The runtime adds no sync or authorization mechanics of
//! its own: it registers what it hosts with data-layer's access book.

pub mod connections;
pub mod data;
mod hosted;
pub mod identity;
pub mod linking;
pub mod pairing;
pub mod retraction;
pub mod runtime;
pub mod sync;

pub use connections::{
    ConnectionsService, DelegationUnsupported, PeerGrant, PeerNotConnected,
    RuntimeConnectionsService,
};
pub use data::{DataService, RuntimeDataService, WriteNotGranted};
// Vocabulary re-exports, so hosts depend on `pdn-node` alone.
pub use data_layer::{
    claim_id_of, CatchUpTimeout, Connectivity, DirectoryHeld, DocTicket, GrantedClaim, ReadGrant,
    ShareMode, SpawnOptions, StorageConfig, UnknownIssuer,
};
pub use identity::{IdentityService, RuntimeIdentityService};
pub use linking::{
    DialogueTimeout, IdentityAlreadyHosted, LinkingInProgress, LinkingLocalFailure, LinkingPayload,
    LinkingRefused, UnsupportedLinkingVersion, LINKING_FORMAT_VERSION,
};
pub use pairing::{
    EstablishmentInProgress, EstablishmentRefused, EstablishmentTimeout, InvitePayload,
    InviterUnreachable, UnsupportedInviteVersion, INVITE_FORMAT_VERSION,
};
pub use pdn_types::{ClaimId, EntryInfo, EntryPath, NodeId, NonEmpty, PdnId};
pub use retraction::RetractionEvent;
pub use runtime::{Runtime, UnknownIdentity};
pub use sync::{RuntimeSyncService, SyncService};
