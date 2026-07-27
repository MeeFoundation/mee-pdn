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
use pdn_store::{
    api::Doc, store::Query, AuthorId, EntryFilter, NamespaceId, SessionAccess, SessionRole,
    ValidateOutcome,
};
use pdn_types::{ClaimId, NodeId, PdnId};

use crate::connection_metadata::GrantRecord;
use crate::grant::{claim_id_of_key, GrantedClaim, ReadGrant};
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

/// What one connection grants on one issuer's data: exactly these claims —
/// each with its own commands — or nothing. Every grant is
/// capability-scoped, so a granted session is always a filtered one — no
/// branch reaches the full view through a grant.
enum GrantWidth {
    /// A grant: exactly these claims, commands per claim.
    Claims(Vec<GrantedClaim>),
    /// No grant recorded.
    None,
}

/// A caller's effective rights on one issuer's data: the union of the
/// grants every matching connection carries. `read` drives the egress
/// filter; `write` drives the ingest gate. Write never exceeds read — a
/// granted claim always grants read.
#[derive(Debug, Default)]
pub(crate) struct EffectiveRights {
    pub(crate) read: HashSet<ClaimId>,
    pub(crate) write: HashSet<ClaimId>,
}

impl EffectiveRights {
    fn extend(&mut self, claims: Vec<GrantedClaim>) {
        for granted in claims {
            self.read.insert(granted.claim);
            if granted.write {
                self.write.insert(granted.claim);
            }
        }
    }
}

/// What one classified session may write into a hosted issuer's replica —
/// the write-side half of a classification, deposited per `(replica,
/// peer)` at session setup and consulted synchronously by the ingest gate.
/// Rights are frozen per session for writes exactly as for reads: a later
/// session overwrites the deposit.
#[derive(Debug)]
enum WriteAdmission {
    /// The issuer's own device: every entry is admitted.
    Full,
    /// A granted audience device: exactly these claims.
    Claims(HashSet<ClaimId>),
}

/// One armed retraction: the timestamp bound per retracted `(author, key)`
/// of one granted namespace. An ingested entry matching an armed pair at
/// or below its bound is refused, so a retracted provisional write cannot
/// flap back from a sibling that still holds it.
type ArmedRetractions = HashMap<(AuthorId, Vec<u8>), u64>;

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
    /// Per-session write admissions, deposited by [`Self::classify`] for
    /// hosted-issuer data replicas and consulted synchronously by the
    /// ingest gate ([`Self::admit_ingest`]) — the classifier is the async
    /// half that reads grant records, the gate the sync half that only
    /// looks up.
    write_sets: RwLock<HashMap<(NamespaceId, NodeId), WriteAdmission>>,
    /// Armed retraction markers per namespace, consulted by the ingest gate
    /// on data replicas — the only replicas whose entries a marker can name.
    retractions: RwLock<HashMap<NamespaceId, ArmedRetractions>>,
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
            return self
                .classify_data(namespace, issuer, posture, caller, role)
                .await;
        }

        // Unknown to the book entirely: ticket possession is the only
        // bound.
        Ok(SessionAccess::Full)
    }

    async fn classify_data(
        &self,
        namespace: NamespaceId,
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
        // hosted directory. Each verdict deposits the session's write
        // admission for the ingest gate — the async half computing what the
        // sync half only looks up.
        if let Some(directory) = self.directory_of(issuer)? {
            if device_listed(&directory, caller_key.as_bytes()).await? {
                self.deposit_write_admission(namespace, caller, WriteAdmission::Full)?;
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
            let rights = self
                .union_rights(caller_key.as_bytes(), issuer, grant_key.as_bytes(), grants)
                .await?;
            if rights.read.is_empty() {
                return Ok(SessionAccess::Deny);
            }
            self.deposit_write_admission(namespace, caller, WriteAdmission::Claims(rights.write))?;
            return Ok(SessionAccess::Filtered(egress_filter(issuer, rights.read)));
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
                let rights = self
                    .union_rights(caller_key.as_bytes(), issuer, grant_key.as_bytes(), grants)
                    .await?;
                if !rights.read.is_empty() {
                    return Ok(SessionAccess::Filtered(egress_filter(issuer, rights.read)));
                }
                Ok(match role {
                    SessionRole::Accept => SessionAccess::Deny,
                    SessionRole::Dial => SessionAccess::Filtered(closed_egress()),
                })
            }
            ServingPosture::Serve => Ok(SessionAccess::Full),
        }
    }

    /// The union of the rights every listed grant carries for the caller —
    /// read and write claim sets side by side. Each item is `(probe,
    /// grant_doc, audience)`: the caller must be a device listed in
    /// `probe`, and the claims come from `grant_doc`'s one grant record
    /// only when its capability names this `issuer` and this `audience`. A
    /// caller absent from a probe, or a grant record absent / still
    /// replicating / addressed elsewhere, contributes nothing; an empty
    /// read union means the caller has no computable grant. Both the hosted
    /// side (classifying its counterparty) and the grantee side (classifying
    /// a sibling) reduce to this — they differ only in which doc probes and
    /// which carries the grant.
    async fn union_rights(
        &self,
        caller_key: &[u8],
        issuer: PdnId,
        grant_key: &[u8],
        grants: impl IntoIterator<Item = (Doc, Doc, PdnId)>,
    ) -> Result<EffectiveRights> {
        let mut rights = EffectiveRights::default();
        for (probe, grant_doc, audience) in grants {
            if !device_listed(&probe, caller_key).await? {
                continue;
            }
            if let GrantWidth::Claims(grant_claims) = self
                .grant_width_in(&grant_doc, issuer, audience, grant_key)
                .await?
            {
                rights.extend(grant_claims);
            }
        }
        Ok(rights)
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

    /// Record what a classified session may write into `namespace` —
    /// overwriting any previous deposit for the same `(namespace, caller)`:
    /// rights are frozen per session, and the newest session's setup is the
    /// one whose entries can still arrive.
    fn deposit_write_admission(
        &self,
        namespace: NamespaceId,
        caller: NodeId,
        admission: WriteAdmission,
    ) -> Result<()> {
        self.write_sets
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("write admissions lock poisoned"))?
            .insert((namespace, caller), admission);
        Ok(())
    }

    /// The synchronous ingest verdict for one entry arriving from `from` —
    /// the write-side counterpart of [`Self::classify`], consulted by the
    /// fork inside its insert path, so nothing here may block or read a
    /// replica. Data replicas are judged: their retraction markers refuse
    /// their exact entries, and on the ones data-bound to an identity this
    /// node hosts the caller's session deposit decides (its absence is
    /// fail-closed — no classified session, no admission). Every other
    /// replica — directories, connection metadata stores, and replicas held
    /// under someone else's issuance — admits as it always has, bounded by
    /// the serving side's egress and by ticket possession.
    ///
    /// Only a live capability verdict against the caller's deposit is
    /// [`ValidateOutcome::Reject`], the outcome the fork echoes back for the
    /// sender to retract on. Everything else this gate refuses is
    /// [`ValidateOutcome::Drop`]: a marker match states what this node
    /// already retracted rather than judging the sender, and a state this
    /// node cannot read judges nobody at all — neither may cost a peer its
    /// own legitimately written entry.
    pub(crate) fn admit_ingest(
        &self,
        registry: &Registry,
        entry: &pdn_store::SignedEntry,
        from: &pdn_store::PeerIdBytes,
    ) -> ValidateOutcome {
        let id = entry.id();
        let namespace = id.namespace();
        let issuer = match registry.binding_of(namespace) {
            Ok(Some((issuer, _posture))) => issuer,
            // Not a data replica on this node: directories and connection
            // metadata stores keep their ticket-bounded admission. Markers
            // are consulted below this exit — they can only name entries of
            // a data replica, and consulting them here would let one
            // unreadable map silence the stores linking and pairing stand on.
            Ok(None) => return ValidateOutcome::Accept,
            Err(_poisoned) => return ValidateOutcome::Drop,
        };
        if self.retraction_names(namespace, entry) {
            return ValidateOutcome::Drop;
        }
        match self.directory_of(issuer) {
            // Not hosted here — a grantee-held or ticket-bounded replica:
            // inbound entries are already bounded by the serving side's
            // egress filter.
            Ok(None) => return ValidateOutcome::Accept,
            Ok(Some(_directory)) => {}
            Err(_poisoned) => return ValidateOutcome::Drop,
        }
        let caller = NodeId::from_bytes(*from);
        let Ok(admissions) = self.write_sets.read() else {
            return ValidateOutcome::Drop;
        };
        match admissions.get(&(namespace, caller)) {
            Some(WriteAdmission::Full) => ValidateOutcome::Accept,
            Some(WriteAdmission::Claims(claims)) => {
                if covers_key(claims, issuer, id.key()) {
                    ValidateOutcome::Accept
                } else {
                    ValidateOutcome::Reject
                }
            }
            // No classified session: this node's own state, not a verdict on
            // the caller's authority.
            None => ValidateOutcome::Drop,
        }
    }

    /// Arm a retraction marker: entries of `author` at `key` in `namespace`
    /// with a timestamp at or below `bound` are refused at ingest. A wider
    /// bound replaces a narrower one, never the reverse — the widest known
    /// retraction is the one that must hold.
    pub(crate) fn arm_retraction(
        &self,
        namespace: NamespaceId,
        author: AuthorId,
        key: Vec<u8>,
        bound: u64,
    ) -> Result<()> {
        let mut retractions = self
            .retractions
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("retractions lock poisoned"))?;
        let armed = retractions.entry(namespace).or_default();
        let slot = armed.entry((author, key)).or_insert(bound);
        *slot = (*slot).max(bound);
        Ok(())
    }

    /// Drop the armed retraction of one `(author, key)` in `namespace` — the
    /// counterpart of the marker that armed it being dropped. An armed pair
    /// outlives nothing else: the durable marker is what re-arms it on every
    /// sweep, so a refusal left standing after its marker aged out would
    /// answer for as long as the process runs.
    pub(crate) fn disarm_retraction(
        &self,
        namespace: NamespaceId,
        author: AuthorId,
        key: &[u8],
    ) -> Result<()> {
        let mut retractions = self
            .retractions
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("retractions lock poisoned"))?;
        if let Some(armed) = retractions.get_mut(&namespace) {
            armed.remove(&(author, key.to_vec()));
            if armed.is_empty() {
                retractions.remove(&namespace);
            }
        }
        Ok(())
    }

    /// Drop every armed retraction of `namespace` — the counterpart of
    /// forgetting the granted namespace: the entries the markers address
    /// left with the replica.
    pub(crate) fn disarm_retractions(&self, namespace: NamespaceId) -> Result<()> {
        self.retractions
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("retractions lock poisoned"))?
            .remove(&namespace);
        Ok(())
    }

    /// Whether an armed retraction names this entry: same author, same key,
    /// timestamp at or below the bound.
    fn retraction_names(&self, namespace: NamespaceId, entry: &pdn_store::SignedEntry) -> bool {
        let Ok(retractions) = self.retractions.read() else {
            // Cannot read what was retracted: claiming a match is the
            // fail-closed side, and it costs the sender nothing — the gate
            // answers a marker match with a silent drop.
            return true;
        };
        let Some(armed) = retractions.get(&namespace) else {
            return false;
        };
        let id = entry.id();
        armed
            .get(&(id.author(), id.key().to_vec()))
            .is_some_and(|bound| entry.timestamp() <= *bound)
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

/// Whether a claim set covers the entry at raw `key` in `issuer`'s replica —
/// evaluated in the reverse direction, no id-to-location mapping. The one
/// derivation both directions of enforcement use: the egress filter admits by
/// it, the ingest gate admits by it, and a drift between the two is exactly
/// the read/write asymmetry the capability scoping exists to prevent.
///
/// The fork requires this cheap (it runs on every entry a range scan
/// touches), so the test is the raw-key derivation: no per-entry parse, no
/// allocation — a key that is not a valid path derives an id no grant
/// contains and is excluded, the same verdict parsing first would reach
/// ([`claim_id_of_key`]).
fn covers_key(claims: &HashSet<ClaimId>, issuer: PdnId, key: &[u8]) -> bool {
    claims.contains(&claim_id_of_key(&issuer, key))
}

/// The egress filter for a session: admit exactly the entries the session's
/// read claims cover.
fn egress_filter(issuer: PdnId, claims: HashSet<ClaimId>) -> EntryFilter {
    Arc::new(move |entry: &pdn_store::SignedEntry| covers_key(&claims, issuer, entry.id().key()))
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

/// Build the fork's ingest validator over the same book and registry — the
/// write-side counterpart of [`session_access_provider`], installed beside
/// it at spawn (ADR-0008). The classifier deposits each session's write
/// admission; this validator only looks it up, per entry, synchronously.
pub(crate) fn capability_ingest_validator(
    book: Arc<AccessBook>,
    registry: Arc<Registry>,
) -> pdn_store::CapabilityValidator {
    Arc::new(move |entry, from| book.admit_ingest(&registry, entry, from))
}

#[cfg(test)]
mod tests {
    use pdn_store::{Author, Entry, NamespaceSecret, Record, RecordIdentifier, SignedEntry};

    use super::*;

    fn signed(
        namespace: &NamespaceSecret,
        author: &Author,
        key: &str,
        timestamp: u64,
    ) -> SignedEntry {
        let id = RecordIdentifier::new(namespace.id(), author.id(), key);
        let record = Record::new(Hash::new(b"payload"), 7, timestamp);
        SignedEntry::from_entry(Entry::new(id, record), namespace, author)
    }

    /// Make `lock` unreadable the only way it can become so: an unwind while
    /// a write guard is held.
    fn poison<T: Send>(lock: &RwLock<T>) {
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.write().expect("not poisoned yet");
            std::panic::resume_unwind(Box::new("poison"));
        }));
        assert!(unwound.is_err());
        assert!(lock.read().is_err(), "the lock is poisoned");
    }

    /// A marker refuses its exact entry and nothing beyond it: the retracted
    /// timestamp and everything under it match, a newer own write at the same
    /// author and key does not — writing again after a retraction is the way
    /// back — and an entry of another author at that path (the issuer's own)
    /// is never matched.
    #[test]
    fn a_marker_names_its_entry_up_to_the_bound_only() {
        let namespace = NamespaceSecret::from_bytes(&[7u8; 32]);
        let author = Author::from_bytes(&[5u8; 32]);
        let book = AccessBook::default();
        book.arm_retraction(namespace.id(), author.id(), b"contact/email".to_vec(), 50)
            .expect("arm");

        let names = |key, timestamp| {
            book.retraction_names(namespace.id(), &signed(&namespace, &author, key, timestamp))
        };
        assert!(names("contact/email", 50), "the retracted entry");
        assert!(names("contact/email", 49), "an older one at the same key");
        assert!(
            !names("contact/email", 51),
            "a newer own write gets through"
        );
        assert!(!names("contact/phone", 50), "another key is untouched");

        // The marker addresses one author. The issuer's own entries at the
        // very same path carry a different author and are never matched — the
        // property the partial-withdrawal case leans on (a marker for a
        // racing own write must not suppress the issuer's later value there).
        let issuer_author = Author::from_bytes(&[6u8; 32]);
        assert!(
            !book.retraction_names(
                namespace.id(),
                &signed(&namespace, &issuer_author, "contact/email", 50)
            ),
            "another author's entry at the marked path is untouched"
        );

        book.disarm_retractions(namespace.id()).expect("disarm");
        assert!(!names("contact/email", 50), "disarmed");
    }

    /// A wider bound replaces a narrower one, never the reverse.
    #[test]
    fn arming_widens_and_never_narrows() {
        let namespace = NamespaceSecret::from_bytes(&[7u8; 32]);
        let author = Author::from_bytes(&[5u8; 32]);
        let book = AccessBook::default();
        let arm = |bound| {
            book.arm_retraction(
                namespace.id(),
                author.id(),
                b"contact/email".to_vec(),
                bound,
            )
            .expect("arm");
        };
        arm(50);
        arm(10);
        assert!(
            book.retraction_names(
                namespace.id(),
                &signed(&namespace, &author, "contact/email", 50)
            ),
            "the wider bound of the two holds"
        );
    }

    /// Markers are consulted only where they can name an entry — on data
    /// replicas. A retraction map this node cannot read refuses those
    /// (silently, costing the sender nothing), and leaves every other replica
    /// — directories, connection metadata stores — admitting as before, so one
    /// unreadable map cannot stop the records linking and pairing stand on.
    #[test]
    fn an_unreadable_retraction_map_reaches_no_further_than_data_replicas() {
        let namespace = NamespaceSecret::from_bytes(&[7u8; 32]);
        let author = Author::from_bytes(&[5u8; 32]);
        let book = AccessBook::default();
        poison(&book.retractions);
        assert!(
            book.retraction_names(namespace.id(), &signed(&namespace, &author, "devices/x", 1)),
            "unreadable: claiming the match is the fail-closed side"
        );

        // No binding: the namespace is not a data replica of this node.
        let registry = Registry::default();
        assert_eq!(
            book.admit_ingest(
                &registry,
                &signed(&namespace, &author, "devices/x", 1),
                &[9u8; 32]
            ),
            ValidateOutcome::Accept
        );
    }
}
