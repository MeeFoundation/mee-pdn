//! Ingest gate: domain-level admission policy and its bridge into the
//! fork's `CapabilityValidator` hook.
//!
//! The fork's hook sees `&SignedEntry` and returns `bool` — it knows nothing
//! about `MeeIds`. This module translates: resolve the entry's iroh namespace
//! to a domain [`NamespaceId`] via the shared registry index, hand the
//! domain view to an [`IngestPolicy`], and map the verdict back to `bool`.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError, RwLock};

use iroh_docs::{CapabilityValidator, SignedEntry};
use mee_sync_api::NamespaceId;
use mee_types::MeeId;

use crate::registry::NamespaceIndex;

/// Verdict of an [`IngestPolicy`] for one incoming entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Persist the entry: it merges into the replica.
    Accept,
    /// Drop the entry: it is never stored.
    Reject,
}

/// What the gate knows about an incoming entry, in domain terms.
///
/// Non-exhaustive: fields grow as richer policies (`UWill` chains) need more
/// context — entry path, author, origin peer.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct IngestCtx {
    /// The domain namespace the entry belongs to, if the backing iroh
    /// namespace is bound in the local registry; `None` for unknown docs.
    ///
    /// Why `Option`:
    /// - the hook is global per docs engine: entries from replicas unbound in the registry exist;
    /// - "unknown → reject" is itself a verdict — verdicts live in the policy, not the bridge;
    /// - `import` starts syncing before `bind` runs, so a gate hit in that gap has no mapping yet.
    pub namespace: Option<NamespaceId>,
}

/// Domain-level admission policy consulted for every incoming (non-local)
/// entry before it is persisted.
///
/// # Execution context
///
/// The check runs synchronously on the sync-actor thread, ahead of the
/// store `put`. Implementations must not block or perform I/O: everything
/// a decision needs (connection sets, capability indexes) must already be
/// in memory. In particular, a decision can never depend on the entry's
/// blob content — only its record metadata.
pub trait IngestPolicy: Send + Sync + 'static {
    /// Decide whether the entry described by `ctx` may be persisted.
    fn admit(&self, ctx: &IngestCtx) -> Admission;
}

/// Bridge a domain [`IngestPolicy`] into the fork's iroh-native hook.
pub(crate) fn capability_validator(
    policy: Arc<dyn IngestPolicy>,
    index: NamespaceIndex,
) -> CapabilityValidator {
    Arc::new(move |entry: &SignedEntry| {
        let ctx = IngestCtx {
            namespace: index.resolve(entry.entry().namespace()),
        };
        policy.admit(&ctx) == Admission::Accept
    })
}

/// A node's live set of connections: the [`MeeId`]s it currently accepts
/// entries from.
///
/// Cheaply cloneable handle around shared state: the application side
/// mutates it at runtime while the gate reads it from the sync-actor
/// thread.
#[derive(Clone, Debug, Default)]
pub struct Connections {
    inner: Arc<RwLock<HashSet<MeeId>>>,
}

impl Connections {
    /// Create an empty connection set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `id` to the set; returns `false` if it was already present.
    pub fn insert(&self, id: MeeId) -> bool {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id)
    }

    /// Remove `id` from the set; returns `true` if it was present.
    pub fn remove(&self, id: &MeeId) -> bool {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
    }

    /// Whether `id` is currently in the set.
    pub fn contains(&self, id: &MeeId) -> bool {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(id)
    }
}

/// Naive peering policy: admit an entry iff the issuer of its namespace is
/// a current connection — the single-link, degenerate form of a capability
/// chain (ADR-0008). Entries of unknown namespaces are rejected.
#[derive(Clone, Debug)]
pub struct ConnectionsPolicy {
    connections: Connections,
}

impl ConnectionsPolicy {
    /// Build the policy around a shared, runtime-mutable connection set.
    pub fn new(connections: Connections) -> Self {
        Self { connections }
    }
}

impl IngestPolicy for ConnectionsPolicy {
    fn admit(&self, ctx: &IngestCtx) -> Admission {
        match &ctx.namespace {
            Some(ns) if self.connections.contains(&ns.issued_by) => Admission::Accept,
            _ => Admission::Reject,
        }
    }
}
