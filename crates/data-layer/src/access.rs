//! Caller classification for reconciliation sessions: what one session may
//! see of a replica, decided from material this node already holds — the
//! directories of the identities it hosts, and the connection metadata
//! pairs of those identities (grants, published device sets). Nothing is
//! presented over the wire; the transport-authenticated caller node id and
//! the requested namespace are the only inputs.
//!
//! Enforcement is armed per identity by registration
//! ([`SyncNode::host_identity`](crate::SyncNode::host_identity) /
//! [`host_connection`](crate::SyncNode::host_connection)) and per replica by
//! the scoped import
//! ([`import_namespace_scoped`](crate::SyncNode::import_namespace_scoped)).
//! A replica the book knows nothing about is served whole to any ticket
//! holder.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::Result;
use iroh_blobs::Hash;
use pdn_store::{api::Doc, store::Query, EntryFilter, NamespaceId, SessionAccess, SessionRole};
use pdn_types::{ClaimId, NodeId, PdnId};

use crate::connection_metadata::GrantRecord;
use crate::grant::{claim_id_of_key, ReadGrant};
use crate::registry::{Registry, ServingPosture};

/// One hosted connection: the directional stores of `identity` toward
/// `peer`, registered for classification. `own` carries the grants this
/// identity issued (and its own published device set); `peer_doc` is the
/// counterparty's reverse store — its published device set and its grants.
#[derive(Debug, Clone)]
struct HostedConnection {
    identity: PdnId,
    peer: PdnId,
    own: Doc,
    peer_doc: Doc,
}

/// What one connection grants on one issuer's data: exactly these claims,
/// or nothing. Every grant is capability-scoped, so a granted session is
/// always a filtered one — no branch reaches the full view through a grant.
enum GrantWidth {
    /// A grant: exactly these claims.
    Claims(Vec<ClaimId>),
    /// No grant recorded.
    None,
}

/// The classification material one node holds: directories of hosted
/// identities and connection pairs, consulted per session by the access
/// provider wired into the fork at spawn.
#[derive(Debug, Default)]
pub(crate) struct AccessBook {
    /// identity → its directory replica (device records, Invariant 1).
    directories: RwLock<HashMap<PdnId, Doc>>,
    /// Hosted connections, in registration order.
    connections: RwLock<Vec<HostedConnection>>,
    /// The node's blob store, for payload-carrying reads (grant caps);
    /// set right after the stack spawns, before any session can arrive.
    blobs: OnceLock<iroh_blobs::api::Store>,
    /// Decoded grant records, keyed by the replica they sit in and validated
    /// by the record's content hash. A grant is re-read (`get_one`) every
    /// session to learn its current hash, but the blob fetch and JSON decode
    /// behind it run only on a hash the cache has not seen — the record is
    /// content-addressed, so a hash match is provably the current bytes, and
    /// a republish or withdrawal changes the hash and misses. `None` caches
    /// "these bytes decode to no usable grant"; a payload not yet replicated
    /// is never cached, so it is re-checked until it lands.
    grant_cache: RwLock<HashMap<NamespaceId, (Hash, Option<ReadGrant>)>>,
}

impl AccessBook {
    pub(crate) fn set_blobs(&self, blobs: iroh_blobs::api::Store) {
        // A second set is impossible by construction (one spawn per node);
        // OnceLock ignores it if it ever happened.
        let _ = self.blobs.set(blobs);
    }

    pub(crate) fn host_identity(&self, identity: PdnId, directory: Doc) -> Result<()> {
        self.directories
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .insert(identity, directory);
        Ok(())
    }

    pub(crate) fn unhost_identity(&self, identity: PdnId) -> Result<()> {
        self.directories
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .remove(&identity);
        Ok(())
    }

    pub(crate) fn host_connection(
        &self,
        identity: PdnId,
        peer: PdnId,
        own: Doc,
        peer_doc: Doc,
    ) -> Result<()> {
        let mut connections = self
            .connections
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?;
        // One record per (identity, peer): re-registration (pair reopened,
        // re-establishment onto fresh replicas) replaces, never accretes.
        connections.retain(|c| !(c.identity == identity && c.peer == peer));
        connections.push(HostedConnection {
            identity,
            peer,
            own,
            peer_doc,
        });
        Ok(())
    }

    /// Classify `caller` for `namespace`: full view, filtered view, or no
    /// session. Fail-closed wherever the book can judge; a namespace the
    /// book knows nothing about is served whole.
    pub(crate) async fn classify(
        &self,
        registry: &Registry,
        namespace: NamespaceId,
        caller: NodeId,
        role: SessionRole,
    ) -> SessionAccess {
        match self.try_classify(registry, namespace, caller, role).await {
            Ok(access) => access,
            // A classification that failed to read its own material serves
            // nothing rather than everything.
            Err(_storage_error) => SessionAccess::Deny,
        }
    }

    async fn try_classify(
        &self,
        registry: &Registry,
        namespace: NamespaceId,
        caller: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        // Directory and connection-metadata replicas are ticket-gated
        // (Invariants 1 and 3): possession of the ticket is their enforcing
        // mechanism, the whole audience reads them whole, and the filter
        // has nothing to narrow there. Classifying them against their own —
        // possibly not yet converged — device records would deadlock the
        // very bootstrap that delivers those records: a fresh import must
        // sync before it can know who its peers are.
        if self.directory_by_namespace(namespace)?.is_some()
            || self.connection_by_namespace(namespace)?.is_some()
        {
            return Ok(SessionAccess::Full);
        }

        // A data replica known to the registry.
        if let Some((issuer, posture)) = registry.binding_of(namespace)? {
            return self.classify_data(issuer, posture, caller, role).await;
        }

        // Unknown to the book entirely: ticket possession is the only
        // bound.
        Ok(SessionAccess::Full)
    }

    async fn classify_data(
        &self,
        issuer: PdnId,
        posture: ServingPosture,
        caller: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        // `caller` and `issuer` are fixed for the whole classification, so
        // their lookup keys are derived once and reused across every probe
        // and grant read below rather than re-encoded per connection.
        let caller_key = crate::private_metadata::device_key(&caller);
        let grant_key = crate::connection_metadata::grant_key(&issuer);

        // The issuer's own devices see everything, judged through the
        // hosted directory.
        if let Some(directory) = self.directory_of(issuer)? {
            if device_listed(&directory, caller_key.as_bytes()).await? {
                return Ok(SessionAccess::Full);
            }
            // Granted counterparties: the union of the grants every matching
            // connection carries for this caller (a device published by two
            // hosted identities gets the union). Each grant is read from the
            // connection's `own` store — where this identity wrote it —
            // gated on the caller being a device the counterparty published.
            let grants = self
                .connections_of_identity(issuer)?
                .into_iter()
                .map(|c| (c.peer_doc, c.own, c.peer));
            let claims = self
                .union_claims(caller_key.as_bytes(), issuer, grant_key.as_bytes(), grants)
                .await?;
            if claims.is_empty() {
                return Ok(SessionAccess::Deny);
            }
            return Ok(SessionAccess::Filtered(egress_filter(issuer, claims)));
        }

        // This node does not host the issuer. A grantee binding still
        // recognizes the issuer's own devices (via their published device
        // set) and gives them the full view. A caller that resolves as a
        // device of the grant's audience identity — through that identity's
        // own directory, never a record a counterparty wrote — is served
        // per the locally replicated grant record: the same claim-set filter
        // the issuer applies, or nothing when the record is absent or
        // withdrawn. Everyone else is refused, uniform with not-hosted: a
        // third party's rights are not computable here. Dialing out toward an
        // unresolved callee keeps a closed egress: serve nothing, receive
        // whatever the callee's own filter admits. A `Serve` binding is the
        // ticket-bounded stance: the whole replica.
        match posture {
            ServingPosture::AudienceDevices => {
                // One pass over the pairs toward the issuer: a caller that is
                // the issuer's own device on any pair gets the full view and
                // short-circuits; otherwise each pair whose audience
                // directory this node holds is collected, to union its grant.
                let mut grants = Vec::new();
                for connection in self.connections_with_peer(issuer)? {
                    if device_listed(&connection.peer_doc, caller_key.as_bytes()).await? {
                        return Ok(SessionAccess::Full);
                    }
                    if let Some(directory) = self.directory_of(connection.identity)? {
                        grants.push((directory, connection.peer_doc, connection.identity));
                    }
                }
                let claims = self
                    .union_claims(caller_key.as_bytes(), issuer, grant_key.as_bytes(), grants)
                    .await?;
                if !claims.is_empty() {
                    return Ok(SessionAccess::Filtered(egress_filter(issuer, claims)));
                }
                Ok(match role {
                    SessionRole::Accept => SessionAccess::Deny,
                    SessionRole::Dial => SessionAccess::Filtered(closed_egress()),
                })
            }
            ServingPosture::Serve => Ok(SessionAccess::Full),
        }
    }

    /// The union of the claims every listed grant carries for the caller.
    /// Each item is `(probe, grant_doc, audience)`: the caller must be a
    /// device listed in `probe`, and the claims come from `grant_doc`'s one
    /// grant record only when its capability names this `issuer` and this
    /// `audience`. A caller absent from a probe, or a grant record absent /
    /// still replicating / addressed elsewhere, contributes nothing; an
    /// empty union means the caller has no computable grant. Both the hosted
    /// side (classifying its counterparty) and the grantee side (classifying
    /// a sibling) reduce to this — they differ only in which doc probes and
    /// which carries the grant.
    async fn union_claims(
        &self,
        caller_key: &[u8],
        issuer: PdnId,
        grant_key: &[u8],
        grants: impl IntoIterator<Item = (Doc, Doc, PdnId)>,
    ) -> Result<HashSet<ClaimId>> {
        let mut claims: HashSet<ClaimId> = HashSet::new();
        for (probe, grant_doc, audience) in grants {
            if !device_listed(&probe, caller_key).await? {
                continue;
            }
            if let GrantWidth::Claims(grant_claims) = self
                .grant_width_in(&grant_doc, issuer, audience, grant_key)
                .await?
            {
                claims.extend(grant_claims);
            }
        }
        Ok(claims)
    }

    /// What one metadata replica records as the grant on `issuer`'s data
    /// toward `audience`, read from the one grant record at `grant_key` —
    /// the connection's `own` store when the issuer side classifies its
    /// counterparty, the replicated `peer` store when a grantee device
    /// classifies a sibling.
    ///
    /// Claims come only from a present, *decoded* record whose capability
    /// names this very issuer and this very audience. Everything else — no
    /// record, a payload still replicating, a record kind this build cannot
    /// decode, a capability addressed elsewhere — is no grant. Nothing may
    /// be inferred from a record's mere presence: the record's position (in
    /// which store it sits) says who wrote it, but only `cap.audience` says
    /// whom it was written *for*, and a node holding two connections onto
    /// one replica cannot tell them apart by position alone.
    async fn grant_width_in(
        &self,
        doc: &Doc,
        issuer: PdnId,
        audience: PdnId,
        grant_key: &[u8],
    ) -> Result<GrantWidth> {
        let Some(blobs) = self.blobs.get() else {
            return Ok(GrantWidth::None);
        };
        let query = Query::single_latest_per_key().key_exact(grant_key);
        let Some(entry) = doc.get_one(query).await? else {
            return Ok(GrantWidth::None);
        };
        let cap = self
            .cached_grant(doc.id(), entry.content_hash(), blobs)
            .await?;
        Ok(match cap {
            Some(cap) if cap.issuer == issuer && cap.audience == audience => {
                GrantWidth::Claims(cap.claims.into_vec())
            }
            Some(_) | None => GrantWidth::None,
        })
    }

    /// The decoded capability of the grant record with content `hash` in
    /// `namespace`, from the cache when its hash matches, else fetched and
    /// decoded and then cached. Returns `None` for a record that decodes to
    /// no usable grant *and* for a payload not yet replicated — but caches
    /// only the former: the same hash will carry real bytes once the payload
    /// lands, so caching the miss would pin it until the record changes.
    async fn cached_grant(
        &self,
        namespace: NamespaceId,
        hash: Hash,
        blobs: &iroh_blobs::api::Store,
    ) -> Result<Option<ReadGrant>> {
        {
            let cache = self
                .grant_cache
                .read()
                .map_err(|_poisoned| anyhow::anyhow!("grant cache lock poisoned"))?;
            if let Some((cached_hash, cap)) = cache.get(&namespace) {
                if *cached_hash == hash {
                    return Ok(cap.clone());
                }
            }
        }
        if !blobs.has(hash).await? {
            return Ok(None);
        }
        let bytes = blobs.get_bytes(hash).await?;
        let cap = crate::connection_metadata::decode_grant_record(&bytes)
            .map(|GrantRecord::Scoped { cap, .. }| cap);
        self.grant_cache
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("grant cache lock poisoned"))?
            .insert(namespace, (hash, cap.clone()));
        Ok(cap)
    }

    fn directory_by_namespace(&self, namespace: NamespaceId) -> Result<Option<Doc>> {
        Ok(self
            .directories
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .values()
            .find(|doc| doc.id() == namespace)
            .cloned())
    }

    fn directory_of(&self, identity: PdnId) -> Result<Option<Doc>> {
        Ok(self
            .directories
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .get(&identity)
            .cloned())
    }

    fn connection_by_namespace(&self, namespace: NamespaceId) -> Result<Option<HostedConnection>> {
        Ok(self
            .connections
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .iter()
            .find(|c| c.own.id() == namespace || c.peer_doc.id() == namespace)
            .cloned())
    }

    fn connections_of_identity(&self, identity: PdnId) -> Result<Vec<HostedConnection>> {
        Ok(self
            .connections
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .iter()
            .filter(|c| c.identity == identity)
            .cloned()
            .collect())
    }

    fn connections_with_peer(&self, peer: PdnId) -> Result<Vec<HostedConnection>> {
        Ok(self
            .connections
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .iter()
            .filter(|c| c.peer == peer)
            .cloned()
            .collect())
    }
}

/// Whether the device whose record key is `device_key` is recorded
/// (record-level, tombstones excluded) in `doc`. `device_key` is
/// [`crate::private_metadata::device_key`] of the caller — the one shared
/// key definition, computed once per classification and reused across every
/// probe: this membership test decides "own device", so it must never drift
/// from what the stores write.
async fn device_listed(doc: &Doc, device_key: &[u8]) -> Result<bool> {
    let query = Query::single_latest_per_key().key_exact(device_key);
    Ok(doc.get_one(query).await?.is_some())
}

/// An egress that admits nothing: dial-side stance of a scoped holder
/// toward callers it cannot resolve — it serves no entry of the slice
/// while still pulling its own updates.
fn closed_egress() -> EntryFilter {
    Arc::new(|_entry: &pdn_store::SignedEntry| false)
}

/// The egress filter for a session: admit an entry iff the claim identity
/// derived from its key is in the granted set — evaluated in the reverse
/// direction, no id-to-location mapping. The fork requires this cheap (it
/// runs on every entry a range scan touches), so the test is the raw-key
/// derivation: no per-entry parse, no allocation — a key that is not a
/// valid path derives an id no grant contains and is excluded, the same
/// verdict parsing first would reach ([`claim_id_of_key`]).
fn egress_filter(issuer: PdnId, claims: HashSet<ClaimId>) -> EntryFilter {
    Arc::new(move |entry: &pdn_store::SignedEntry| {
        claims.contains(&claim_id_of_key(&issuer, entry.id().key()))
    })
}

/// Build the fork's session access provider over this node's book and
/// registry — the single decision point for both session roles.
pub(crate) fn session_access_provider(
    book: Arc<AccessBook>,
    registry: Arc<Registry>,
) -> pdn_store::SessionAccessProvider {
    Arc::new(move |namespace, peer, role| {
        let book = Arc::clone(&book);
        let registry = Arc::clone(&registry);
        let caller = NodeId::from_bytes(*peer.as_bytes());
        Box::pin(async move { book.classify(&registry, namespace, caller, role).await })
    })
}
