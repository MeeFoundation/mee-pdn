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
/// The composition seam: combine Invariant 1 with
/// data-gating policies in one node. Empty `AnyOf` rejects everything.
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

/// Ingest policy enforcing **Invariant 1**: a node admits entries whose
/// replica belongs to its own identity, consulting no store
/// (`mia-docs/openspec/specs/components/pdn-node/invariants.md`).
///
/// Admits its connections store, its private metadata store, and the data
/// namespaces it issues — on an identity match, reading nothing. The match
/// means "this replica belongs to my device set," not a claim of ownership
/// over its contents: a connection is an object between two identities,
/// owned by neither (the `identity` binding field names which identity's
/// private store it is, not who owns the connections). Reading nothing is
/// what lets an identity's state replicate to a freshly linked (still-empty)
/// device and is why it needs no fork change — it inspects only the resolved
/// [`Binding`].
#[derive(Clone, Copy, Debug)]
pub struct SelfOwned {
    me: PdnId,
}

impl SelfOwned {
    /// Build the policy for the local identity `me`.
    pub fn new(me: PdnId) -> Self {
        Self { me }
    }
}

impl IngestPolicy for SelfOwned {
    fn admit(&self, ctx: &IngestCtx) -> Admission {
        match &ctx.binding {
            Some(Binding::Connections { identity }) if *identity == self.me => Admission::Accept,
            Some(Binding::PrivateMetadata { identity }) if *identity == self.me => {
                Admission::Accept
            }
            Some(Binding::Data { issuer }) if *issuer == self.me => Admission::Accept,
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
            Some(Binding::Data { issuer }) if self.connections.contains(issuer) => {
                Admission::Accept
            }
            _ => Admission::Reject,
        }
    }
}
