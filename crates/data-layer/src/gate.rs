//! Ingest gate: domain-level admission policies and their bridge into the
//! fork's `CapabilityValidator` hook.
//!
//! The fork's hook sees `&SignedEntry` and returns `bool` — it knows nothing
//! about `PdnId`s. This module translates: resolve the entry's iroh namespace
//! to a domain [`Binding`] via the shared registry index, hand the domain
//! view to an [`IngestPolicy`], and map the verdict back to `bool`.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError, RwLock};

use pdn_store::{CapabilityValidator, SignedEntry};
use pdn_types::PdnId;

use crate::registry::{Binding, BindingIndex};

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
    /// The resolved [`Binding`] of the entry's replica, if it is bound in the
    /// local registry; `None` for unknown replicas.
    ///
    /// Why `Option`:
    /// - the hook is global per docs engine: entries from replicas unbound in the registry exist;
    /// - "unknown → reject" is itself a verdict — verdicts live in the policy, not the bridge;
    /// - `import` starts syncing before `bind` runs, so a gate hit in that gap has no mapping yet.
    pub binding: Option<Binding>,
}

/// Domain-level admission policy consulted for every incoming (non-local)
/// entry before it is persisted.
///
/// # Execution context
///
/// The check runs synchronously on the sync-actor thread, ahead of the
/// store `put`. Implementations must not block or perform I/O: everything
/// a decision needs must already be in memory. In particular, a decision
/// can never depend on the entry's blob content — only its record metadata.
pub trait IngestPolicy: Send + Sync + 'static {
    /// Decide whether the entry described by `ctx` may be persisted.
    fn admit(&self, ctx: &IngestCtx) -> Admission;
}

/// Bridge a domain [`IngestPolicy`] into the fork's iroh-native hook.
pub(crate) fn capability_validator(
    policy: Arc<dyn IngestPolicy>,
    index: BindingIndex,
) -> CapabilityValidator {
    Arc::new(move |entry: &SignedEntry| {
        let ctx = IngestCtx {
            binding: index.resolve(entry.entry().namespace()),
        };
        policy.admit(&ctx) == Admission::Accept
    })
}

/// Admit an entry when any composed policy admits it (first-accept).
///
/// The composition seam: combine the device axiom with data-gating policies
/// in one node. Empty `AnyOf` rejects everything.
pub struct AnyOf {
    policies: Vec<Box<dyn IngestPolicy>>,
}

impl AnyOf {
    /// Build from a list of policies, tried in order until one accepts.
    pub fn new(policies: Vec<Box<dyn IngestPolicy>>) -> Self {
        Self { policies }
    }
}

impl IngestPolicy for AnyOf {
    fn admit(&self, ctx: &IngestCtx) -> Admission {
        for policy in &self.policies {
            if policy.admit(ctx) == Admission::Accept {
                return Admission::Accept;
            }
        }
        Admission::Reject
    }
}

/// Device axiom: a node admits entries of bindings owned by its own identity
/// — its connections store and its own data namespaces — **without reading
/// any store**.
///
/// This is what lets an identity's state replicate between its devices: a
/// node always trusts state authored under its own `PdnId`, so gating that
/// on a store would be circular (the store cannot authorize its own
/// arrival). Being read-free is also why it needs no fork change — it
/// inspects only the resolved [`Binding`].
#[derive(Clone, Copy, Debug)]
pub struct SelfOwned {
    me: PdnId,
}

impl SelfOwned {
    /// Build the axiom for the local identity `me`.
    pub fn new(me: PdnId) -> Self {
        Self { me }
    }
}

impl IngestPolicy for SelfOwned {
    fn admit(&self, ctx: &IngestCtx) -> Admission {
        match &ctx.binding {
            Some(Binding::Connections { owner }) if *owner == self.me => Admission::Accept,
            Some(Binding::Data(ns)) if ns.issued_by == self.me => Admission::Accept,
            _ => Admission::Reject,
        }
    }
}

/// A node's live set of connections: the [`PdnId`]s it currently accepts
/// data entries from.
///
/// Cheaply cloneable handle around shared state: the application side
/// mutates it at runtime while the gate reads it from the sync-actor
/// thread. This is the naive, in-memory mechanism; a store-reading
/// successor (the gate consulting the replicated connections store) is a
/// deferred follow-up.
#[derive(Clone, Debug, Default)]
pub struct Connections {
    inner: Arc<RwLock<HashSet<PdnId>>>,
}

impl Connections {
    /// Create an empty connection set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `id` to the set; returns `false` if it was already present.
    pub fn insert(&self, id: PdnId) -> bool {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id)
    }

    /// Remove `id` from the set; returns `true` if it was present.
    pub fn remove(&self, id: &PdnId) -> bool {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
    }

    /// Whether `id` is currently in the set.
    pub fn contains(&self, id: &PdnId) -> bool {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(id)
    }
}

/// Naive peering policy: admit a data-namespace entry iff the issuer of its
/// namespace is a current connection — the single-link, degenerate form of a
/// capability chain (ADR-0008). Entries of other bindings are rejected.
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
        match &ctx.binding {
            Some(Binding::Data(ns)) if self.connections.contains(&ns.issued_by) => {
                Admission::Accept
            }
            _ => Admission::Reject,
        }
    }
}
