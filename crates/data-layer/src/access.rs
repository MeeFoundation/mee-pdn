//! Caller classification for reconciliation sessions, decided from material
//! this node already holds — hosted identities' directories and connection
//! metadata pairs. Nothing is presented over the wire: the
//! transport-authenticated caller node id and the requested namespace are
//! the only inputs. A replica the book knows nothing about is served whole
//! to any ticket holder.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock, RwLock},
};

use anyhow::Result;
use iroh_blobs::Hash;
use pdn_store::{
    api::Doc, store::Query, AuthorId, EntryFilter, NamespaceId, SessionAccess, SessionRole,
    ValidateOutcome,
};
use pdn_types::{ClaimId, NodeId, PdnId};

use crate::{
    connection_metadata::GrantRecord,
    grant::{claim_id_of_key, GrantedClaim, ReadGrant},
    registry::{Registry, ServingPosture},
};

/// The directional stores of `identity` toward `peer`: `own` carries the
/// grants this identity issued, `peer_doc` the counterparty's published
/// device set and grants.
#[derive(Debug, Clone)]
struct HostedConnection {
    identity: PdnId,
    peer: PdnId,
    own: Doc,
    peer_doc: Doc,
}

/// What one connection grants on one issuer's data. No branch reaches the
/// full view through a grant.
enum GrantWidth {
    Claims(Vec<GrantedClaim>),
    None,
}

/// The union of the grants every matching connection carries. Write never
/// exceeds read.
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

/// The write-side half of a classification, deposited per `(replica, peer)`
/// at session setup; a later session overwrites it.
#[derive(Debug)]
enum WriteAdmission {
    Full,
    Claims(HashSet<ClaimId>),
}

/// Timestamp bound per retracted `(author, key)` of one granted namespace.
type ArmedRetractions = HashMap<(AuthorId, Vec<u8>), u64>;

/// The classification material one node holds, consulted per session by
/// the access provider wired into the fork at spawn.
#[derive(Debug, Default)]
pub(crate) struct AccessBook {
    directories: RwLock<HashMap<PdnId, Doc>>,
    connections: RwLock<Vec<HostedConnection>>,
    /// Set right after the stack spawns, before any session can arrive.
    blobs: OnceLock<iroh_blobs::api::Store>,
    /// Decoded grant records keyed by replica, validated by content hash: a
    /// republish or withdrawal changes the hash and misses. A payload not
    /// yet replicated is never cached, so it is re-checked every session.
    grant_cache: RwLock<HashMap<NamespaceId, (Hash, Option<ReadGrant>)>>,
    /// The classifier is the async half that reads grant records; the ingest
    /// gate is the sync half that only looks up.
    write_sets: RwLock<HashMap<(NamespaceId, NodeId), WriteAdmission>>,
    /// Consulted on data replicas only — the only replicas whose entries a
    /// marker can name.
    retractions: RwLock<HashMap<NamespaceId, ArmedRetractions>>,
}

impl AccessBook {
    pub(crate) fn set_blobs(&self, blobs: iroh_blobs::api::Store) {
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
        // One record per (identity, peer): re-registration replaces.
        connections.retain(|c| !(c.identity == identity && c.peer == peer));
        connections.push(HostedConnection {
            identity,
            peer,
            own,
            peer_doc,
        });
        Ok(())
    }

    /// Fail-closed wherever the book can judge; a namespace it knows nothing
    /// about is served whole.
    pub(crate) async fn classify(
        &self,
        registry: &Registry,
        namespace: NamespaceId,
        caller: NodeId,
        role: SessionRole,
    ) -> SessionAccess {
        match self.try_classify(registry, namespace, caller, role).await {
            Ok(access) => access,
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
        // Directories and connection metadata stores are ticket-gated
        // (Invariants 1 and 3). Classifying them against their own, possibly
        // not yet converged, device records would deadlock the bootstrap
        // that delivers those records.
        if self.directory_by_namespace(namespace)?.is_some()
            || self.connection_by_namespace(namespace)?.is_some()
        {
            return Ok(SessionAccess::Full);
        }

        if let Some((issuer, posture)) = registry.binding_of(namespace)? {
            return self
                .classify_data(namespace, issuer, posture, caller, role)
                .await;
        }

        // Unknown to the book: ticket possession is the only bound.
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
        let caller_key = crate::private_metadata::device_key(&caller);
        let grant_key = crate::connection_metadata::grant_key(&issuer);

        // Hosted issuer: own devices see everything; counterparties get the
        // union of the grants every matching connection carries, each read
        // from the connection's `own` store and gated on the caller being a
        // device the counterparty published.
        if let Some(directory) = self.directory_of(issuer)? {
            if device_listed(&directory, caller_key.as_bytes()).await? {
                self.deposit_write_admission(namespace, caller, WriteAdmission::Full)?;
                return Ok(SessionAccess::Full);
            }
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

        // Not hosted here. A grantee binding gives the issuer's own devices
        // (per their published set) the full view, serves a device of the
        // grant's audience identity — resolved through that identity's own
        // directory, never a counterparty-written record — per the local
        // grant record, and refuses everyone else uniformly with not-hosted.
        // Dialing out toward an unresolved callee keeps a closed egress:
        // serve nothing, receive what the callee's own filter admits.
        match posture {
            ServingPosture::AudienceDevices => {
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

    /// The union of the rights the listed grants carry for the caller. Each
    /// item is `(probe, grant_doc, audience)`: the caller must be a device
    /// listed in `probe`, and the claims come from `grant_doc`'s grant
    /// record only when its capability names this `issuer` and `audience`.
    /// The hosted side and the grantee side differ only in which doc probes
    /// and which carries the grant.
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

    /// The grant on `issuer`'s data toward `audience` as one metadata replica
    /// records it. Claims come only from a present, decoded record whose
    /// capability names this very issuer and audience: position says who
    /// wrote a record, only `cap.audience` says whom it was written for.
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

    /// `None` both for bytes that decode to no usable grant and for a
    /// payload not yet replicated — but only the former is cached: the same
    /// hash carries real bytes once the payload lands.
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

    /// The synchronous ingest verdict, consulted by the fork inside its
    /// insert path — nothing here may block or read a replica. Only a
    /// capability verdict against the caller's deposit is
    /// [`ValidateOutcome::Reject`], which the fork echoes back for the sender
    /// to retract on; everything else refused is [`ValidateOutcome::Drop`],
    /// because a marker match or an unreadable state judges nobody and must
    /// not cost a peer its own legitimately written entry.
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
            // Not a data replica: ticket-bounded admission. Markers are
            // consulted below this exit, so one unreadable map cannot silence
            // the stores linking and pairing stand on.
            Ok(None) => return ValidateOutcome::Accept,
            Err(_poisoned) => return ValidateOutcome::Drop,
        };
        if self.retraction_names(namespace, entry) {
            return ValidateOutcome::Drop;
        }
        match self.directory_of(issuer) {
            // Not hosted here: inbound entries are bounded by the serving
            // side's egress filter.
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
            // No classified session: not a verdict on the caller's authority.
            None => ValidateOutcome::Drop,
        }
    }

    /// A wider bound replaces a narrower one, never the reverse.
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

    /// Arming only widens, so without this a refusal whose marker aged out
    /// would answer for as long as the process runs.
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

    pub(crate) fn disarm_retractions(&self, namespace: NamespaceId) -> Result<()> {
        self.retractions
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("retractions lock poisoned"))?
            .remove(&namespace);
        Ok(())
    }

    fn retraction_names(&self, namespace: NamespaceId, entry: &pdn_store::SignedEntry) -> bool {
        let Ok(retractions) = self.retractions.read() else {
            // Fail-closed, and a marker match is a silent drop, so it costs
            // the sender nothing.
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

/// Record-level membership (tombstones excluded). `device_key` is the one
/// shared key definition ([`crate::private_metadata::device_key`]): this test
/// decides "own device", so it must never drift from what the stores write.
async fn device_listed(doc: &Doc, device_key: &[u8]) -> Result<bool> {
    let query = Query::single_latest_per_key().key_exact(device_key);
    Ok(doc.get_one(query).await?.is_some())
}

/// Dial-side stance toward callers a scoped holder cannot resolve: serve
/// nothing while still pulling its own updates.
fn closed_egress() -> EntryFilter {
    Arc::new(|_entry: &pdn_store::SignedEntry| false)
}

/// The one derivation both directions of enforcement use — a drift between
/// them is exactly the read/write asymmetry the scoping exists to prevent.
/// Raw-key, no per-entry parse: the fork runs this on every entry a range
/// scan touches.
fn covers_key(claims: &HashSet<ClaimId>, issuer: PdnId, key: &[u8]) -> bool {
    claims.contains(&claim_id_of_key(&issuer, key))
}

fn egress_filter(issuer: PdnId, claims: HashSet<ClaimId>) -> EntryFilter {
    Arc::new(move |entry: &pdn_store::SignedEntry| covers_key(&claims, issuer, entry.id().key()))
}

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

/// The fork's ingest validator (ADR-0008), installed beside
/// [`session_access_provider`] at spawn.
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

    /// Poison `lock` the only way it can be: an unwind under a write guard.
    fn poison<T: Send>(lock: &RwLock<T>) {
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.write().expect("not poisoned yet");
            std::panic::resume_unwind(Box::new("poison"));
        }));
        assert!(unwound.is_err());
        assert!(lock.read().is_err(), "the lock is poisoned");
    }

    /// A marker refuses its exact entry and nothing beyond it: a newer own
    /// write at the same author and key gets through, and another author at
    /// that path (the issuer's own) is never matched.
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

    /// An unreadable retraction map refuses data replicas (silently) and
    /// leaves every other replica admitting, so it cannot stop the records
    /// linking and pairing stand on.
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
