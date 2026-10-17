//! PDN's document store: multi-writer key-value replicas that two nodes
//! reconcile with range-based set reconciliation, each entry naming its
//! content by hash, with the content itself moved by iroh-blobs. Every
//! session is judged by the embedder through a [`SessionAccessProvider`]
//! (ADR-0014); the crate's rules and their reasons are in its `CLAUDE.md`.
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod metrics;
pub mod net;
pub mod protocol;
mod ticket;

pub mod engine;

pub mod actor;
pub mod api;
pub mod filter;
pub mod store;
pub mod sync;

mod heads;
mod identity;
mod keys;
mod ranger;
mod subscribers;

#[doc(inline)]
pub use net::ALPN;

pub(crate) use self::heads::*;
pub use self::{
    filter::{
        admit_all, EntryFilter, RestsOn, SessionAccess, SessionAccessFuture, SessionAccessProvider,
        SessionIngest, SessionRole,
    },
    identity::{Contact, Identity},
    keys::*,
    ranger::ValidateOutcome,
    sync::*,
    ticket::DocTicket,
};
