//! Caller classification for reconciliation sessions, decided from
//! material one hosted identity already holds — its own directory and its
//! connection metadata pairs. Nothing is presented over the wire: the
//! transport-authenticated caller node id, the holders the session names
//! and the requested namespace are the only inputs. One book per hosted
//! identity, so a verdict is never widened by what a co-located identity
//! holds (ADR-0013).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock, RwLock},
};

use anyhow::Result;
use iroh_blobs::Hash;
use pdn_store::{
    api::Doc, store::Query, AuthorId, EntryFilter, Holder, NamespaceId, SessionAccess,
    SessionIngest, SessionRole, ValidateOutcome,
};
use pdn_types::{ClaimId, NodeId, PdnId};

use crate::{
    connection_metadata::GrantRecord,
    grant::{claim_id_of_key, GrantedClaim, ReadGrant},
    registry::{Registry, ServingPosture},
};

/// A hosted identity as the store names it on the wire: the 32 bytes of
/// its `PdnId`, which the store compares and never interprets.
pub fn holder_of(identity: PdnId) -> Holder {
    Holder::from_bytes(*identity.as_bytes())
}

/// The directional stores of this book's identity toward `peer`: `own`
/// carries the grants it issued, `peer_doc` the counterparty's published
/// device set and grants.
#[derive(Debug, Clone)]
struct HostedConnection {
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

/// The rights one grant record carries. Write never exceeds read.
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

/// The write-side half of a classification. It rides the session it was
/// decided for, so a set frozen at setup holds for that session alone.
#[derive(Debug, Clone)]
enum WriteAdmission {
    /// Everything the peer offers — what an own device is admitted.
    Whole,
    /// Exactly the claims the grant read at setup covers.
    Claims(HashSet<ClaimId>),
    /// Nothing: no session vouched for the writer.
    Nothing,
}

/// Timestamp bound per retracted `(author, key)` of one granted namespace.
type ArmedRetractions = HashMap<(AuthorId, Vec<u8>), u64>;

/// The classification material one hosted identity holds, consulted per
/// session by the access provider its engine was assembled with.
#[derive(Debug)]
pub(crate) struct AccessBook {
    /// Whom this book judges for.
    identity: PdnId,
    /// This identity's own directory, armed by `host_identity`. Until it
    /// is here every data session is refused: the records that judge one
    /// are in it.
    directory: RwLock<Option<Doc>>,
    connections: RwLock<Vec<HostedConnection>>,
    /// Set right after the stack spawns, before any session can arrive.
    blobs: OnceLock<iroh_blobs::api::Store>,
    /// Decoded grant records keyed by replica, validated by content hash: a
    /// republish or withdrawal changes the hash and misses. A payload not
    /// yet replicated is never cached, so it is re-checked every session.
    grant_cache: RwLock<HashMap<NamespaceId, (Hash, Option<ReadGrant>)>>,
    /// Consulted on data replicas only — the only replicas whose entries a
    /// marker can name. Shared, because a session's ingest verdict reads
    /// it per entry and must see what a marker recorded meanwhile.
    retractions: Arc<RwLock<HashMap<NamespaceId, ArmedRetractions>>>,
}

impl AccessBook {
    pub(crate) fn new(identity: PdnId) -> Self {
        Self {
            identity,
            directory: RwLock::new(None),
            connections: RwLock::new(Vec::new()),
            blobs: OnceLock::new(),
            grant_cache: RwLock::new(HashMap::new()),
            retractions: Arc::default(),
        }
    }

    pub(crate) fn set_blobs(&self, blobs: iroh_blobs::api::Store) {
        let _ = self.blobs.set(blobs);
    }

    pub(crate) fn arm_directory(&self, directory: Doc) -> Result<()> {
        *self
            .directory
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))? = Some(directory);
        Ok(())
    }

    pub(crate) fn disarm_directory(&self) -> Result<()> {
        *self
            .directory
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))? = None;
        Ok(())
    }

    pub(crate) fn host_connection(&self, peer: PdnId, own: Doc, peer_doc: Doc) -> Result<()> {
        let mut connections = self
            .connections
            .write()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?;
        // One record per peer: re-registration replaces.
        connections.retain(|c| c.peer != peer);
        connections.push(HostedConnection {
            peer,
            own,
            peer_doc,
        });
        Ok(())
    }

    /// Fail-closed everywhere but the two ticket-bound store kinds
    /// (Invariants 1 and 3).
    ///
    /// `addressed` is the holder whose replica the session names and
    /// `acting` the holder its caller acts for. Which of the two is the
    /// party across the session follows the role: accepting, it is the
    /// caller; dialing, this node is the caller and the party is the
    /// holder it addressed.
    pub(crate) async fn classify(
        &self,
        registry: Arc<Registry>,
        namespace: NamespaceId,
        addressed: Holder,
        acting: Holder,
        peer: NodeId,
        role: SessionRole,
    ) -> SessionAccess {
        let remote = match role {
            SessionRole::Accept => acting,
            SessionRole::Dial => addressed,
        };
        match self
            .try_classify(registry, namespace, remote, peer, role)
            .await
        {
            Ok(access) => access,
            Err(_storage_error) => SessionAccess::Deny,
        }
    }

    async fn try_classify(
        &self,
        registry: Arc<Registry>,
        namespace: NamespaceId,
        remote: Holder,
        peer: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        // The directory and the connection metadata stores are ticket-gated
        // (Invariants 1 and 3). Classifying them against their own, possibly
        // not yet converged, device records would deadlock the bootstrap
        // that delivers those records.
        if self.directory_is(namespace)? || self.connection_by_namespace(namespace)?.is_some() {
            return Ok(SessionAccess::whole());
        }

        let Some((issuer, posture)) = registry.binding_of(namespace)? else {
            // A replica this identity holds under no data binding, or none
            // at all: nothing here can judge the caller, and a ticket bounds
            // no data replica by itself.
            return Ok(SessionAccess::Deny);
        };
        self.classify_data(registry, issuer, posture, remote, peer, role)
            .await
    }

    /// The party across the session names one identity; it is admitted
    /// only when the records this identity holds of that one list the
    /// party's node id. Every path this identity cannot resolve ends in
    /// `refused`: denied when accepting, a closed egress when dialing.
    /// What such a dial then admits follows the posture — nothing on a
    /// replica this identity issues, and on one held under a grant what
    /// the serving side's egress delivers.
    async fn classify_data(
        &self,
        registry: Arc<Registry>,
        issuer: PdnId,
        posture: ServingPosture,
        remote: Holder,
        peer: NodeId,
        role: SessionRole,
    ) -> Result<SessionAccess> {
        let peer_key = crate::private_metadata::device_key(&peer);
        let refused = match role {
            // A dial toward a callee this identity cannot resolve keeps a
            // closed egress: serve nothing, and pull whatever the callee's
            // own filter reveals — admitted or dropped by the posture, as
            // `ingest` decides.
            SessionRole::Dial => SessionAccess::Allow {
                egress: Some(closed_egress()),
                ingest: Some(self.ingest(&registry, WriteAdmission::Nothing)),
            },
            SessionRole::Accept => SessionAccess::Deny,
        };
        match posture {
            ServingPosture::Serve => {
                self.classify_issued(&registry, issuer, remote, peer_key.as_bytes(), refused)
                    .await
            }
            ServingPosture::AudienceDevices => {
                self.classify_held(&registry, issuer, remote, peer_key.as_bytes(), refused)
                    .await
            }
        }
    }

    /// A data replica this identity issues: its own devices see it whole,
    /// a counterparty sees what this identity granted that counterparty.
    async fn classify_issued(
        &self,
        registry: &Arc<Registry>,
        issuer: PdnId,
        remote: Holder,
        peer_key: &[u8],
        refused: SessionAccess,
    ) -> Result<SessionAccess> {
        if remote == holder_of(self.identity) {
            if !self.peer_is_own_device(peer_key).await? {
                return Ok(refused);
            }
            return Ok(SessionAccess::Allow {
                egress: None,
                ingest: Some(self.ingest(registry, WriteAdmission::Whole)),
            });
        }
        let Some(connection) = self.connection_with(remote)? else {
            return Ok(refused);
        };
        // The counterparty's own statement of its devices, which is the
        // only record this identity holds of it.
        if !device_listed(&connection.peer_doc, peer_key).await? {
            return Ok(refused);
        }
        let grant_key = crate::connection_metadata::grant_key(&issuer);
        let rights = self
            .granted_rights(
                &connection.own,
                issuer,
                connection.peer,
                grant_key.as_bytes(),
            )
            .await?;
        if rights.read.is_empty() {
            return Ok(refused);
        }
        Ok(SessionAccess::Allow {
            egress: Some(egress_filter(issuer, rights.read)),
            ingest: Some(self.ingest(registry, WriteAdmission::Claims(rights.write))),
        })
    }

    /// A data replica this identity holds under a grant: the issuer's own
    /// devices see it whole, this identity's own devices see what the
    /// grant covers. What either may put into it is bounded by the
    /// issuer's own gate, so nothing is admitted here beyond the marker
    /// check.
    async fn classify_held(
        &self,
        registry: &Arc<Registry>,
        issuer: PdnId,
        remote: Holder,
        peer_key: &[u8],
        refused: SessionAccess,
    ) -> Result<SessionAccess> {
        let connection = self.connection_with_peer(issuer)?;
        if remote == holder_of(issuer) {
            let listed = match &connection {
                Some(connection) => device_listed(&connection.peer_doc, peer_key).await?,
                None => false,
            };
            if !listed {
                return Ok(refused);
            }
            return Ok(SessionAccess::Allow {
                egress: None,
                ingest: Some(self.ingest(registry, WriteAdmission::Whole)),
            });
        }
        if remote != holder_of(self.identity) || !self.peer_is_own_device(peer_key).await? {
            return Ok(refused);
        }
        let Some(connection) = connection else {
            return Ok(refused);
        };
        let grant_key = crate::connection_metadata::grant_key(&issuer);
        let rights = self
            .granted_rights(
                &connection.peer_doc,
                issuer,
                self.identity,
                grant_key.as_bytes(),
            )
            .await?;
        if rights.read.is_empty() {
            return Ok(refused);
        }
        Ok(SessionAccess::Allow {
            egress: Some(egress_filter(issuer, rights.read)),
            ingest: Some(self.ingest(registry, WriteAdmission::Whole)),
        })
    }

    /// Whether the peer is a device of this book's own identity.
    async fn peer_is_own_device(&self, peer_key: &[u8]) -> Result<bool> {
        let Some(directory) = self.own_directory()? else {
            return Ok(false);
        };
        device_listed(&directory, peer_key).await
    }

    /// The claims one grant record carries toward `audience`. Claims come
    /// only from a present, decoded record whose capability names this very
    /// issuer and audience: position says who wrote a record, only
    /// `cap.audience` says whom it was written for.
    async fn granted_rights(
        &self,
        grant_doc: &Doc,
        issuer: PdnId,
        audience: PdnId,
        grant_key: &[u8],
    ) -> Result<EffectiveRights> {
        let mut rights = EffectiveRights::default();
        if let GrantWidth::Claims(claims) = self
            .grant_width_in(grant_doc, issuer, audience, grant_key)
            .await?
        {
            rights.extend(claims);
        }
        Ok(rights)
    }

    /// The grant on `issuer`'s data toward `audience` as one metadata replica
    /// records it.
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

    /// The session's ingest verdict, consulted by the fork inside its
    /// insert path — nothing here may block or read a replica. Only a
    /// capability verdict against `admission` is
    /// [`ValidateOutcome::Reject`], which the fork echoes back for the
    /// sender to retract on; everything else refused is
    /// [`ValidateOutcome::Drop`], because a marker match or an unreadable
    /// state judges nobody and must not cost a peer its own legitimately
    /// written entry.
    fn ingest(&self, registry: &Arc<Registry>, admission: WriteAdmission) -> SessionIngest {
        let registry = Arc::clone(registry);
        let book = Arc::clone(&self.retractions);
        let identity = self.identity;
        Arc::new(move |entry: &pdn_store::SignedEntry| {
            let id = entry.id();
            let namespace = id.namespace();
            let issuer = match registry.binding_of(namespace) {
                // Not a data replica: ticket-bounded admission. Markers
                // are consulted below this exit, so one unreadable map
                // cannot silence the stores linking and pairing stand on.
                Ok(None) => return ValidateOutcome::Accept,
                Ok(Some((issuer, _posture))) => issuer,
                // An unreadable registry judges nobody: taking it for "no
                // data replica" would admit whatever the session carries.
                Err(_poisoned) => return ValidateOutcome::Drop,
            };
            if retraction_names(&book, namespace, entry) {
                return ValidateOutcome::Drop;
            }
            if issuer != identity {
                // Held under a grant: inbound entries are bounded by the
                // serving side's egress filter.
                return ValidateOutcome::Accept;
            }
            match &admission {
                WriteAdmission::Whole => ValidateOutcome::Accept,
                WriteAdmission::Claims(claims) => {
                    if covers_key(claims, issuer, id.key()) {
                        ValidateOutcome::Accept
                    } else {
                        ValidateOutcome::Reject
                    }
                }
                // No session vouched for the writer: not a verdict on its
                // authority, so the sender re-offers and self-heals.
                WriteAdmission::Nothing => ValidateOutcome::Drop,
            }
        })
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

    #[cfg(test)]
    fn retraction_names(&self, namespace: NamespaceId, entry: &pdn_store::SignedEntry) -> bool {
        retraction_names(&self.retractions, namespace, entry)
    }

    fn own_directory(&self) -> Result<Option<Doc>> {
        Ok(self
            .directory
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .clone())
    }

    fn directory_is(&self, namespace: NamespaceId) -> Result<bool> {
        Ok(self
            .own_directory()?
            .is_some_and(|doc| doc.id() == namespace))
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

    /// The connection with the identity a caller named, if the holder names
    /// one this identity is connected to.
    fn connection_with(&self, holder: Holder) -> Result<Option<HostedConnection>> {
        Ok(self
            .connections
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .iter()
            .find(|c| holder_of(c.peer) == holder)
            .cloned())
    }

    fn connection_with_peer(&self, peer: PdnId) -> Result<Option<HostedConnection>> {
        Ok(self
            .connections
            .read()
            .map_err(|_poisoned| anyhow::anyhow!("access book lock poisoned"))?
            .iter()
            .find(|c| c.peer == peer)
            .cloned())
    }
}

/// Whether an armed marker of `namespace` names this entry — its author,
/// its key, and a timestamp at or below the bound.
fn retraction_names(
    retractions: &RwLock<HashMap<NamespaceId, ArmedRetractions>>,
    namespace: NamespaceId,
    entry: &pdn_store::SignedEntry,
) -> bool {
    let Ok(retractions) = retractions.read() else {
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
    Arc::new(move |namespace, addressed, acting, peer, role| {
        let book = Arc::clone(&book);
        let registry = Arc::clone(&registry);
        let peer = NodeId::from_bytes(*peer.as_bytes());
        Box::pin(async move {
            book.classify(registry, namespace, addressed, acting, peer, role)
                .await
        })
    })
}

/// The fork's standing ingest validator (ADR-0008), installed beside
/// [`session_access_provider`] at spawn. It answers where no session
/// vouched for an entry — a path the platform has none of, since the
/// swarm is content-free and reconciliation is the only ingest — so it
/// refuses anything addressed at a data replica this identity issues and
/// leaves the rest to the ticket bound Invariants 1 and 3 give it.
pub(crate) fn capability_ingest_validator(
    book: &Arc<AccessBook>,
    registry: &Arc<Registry>,
) -> pdn_store::CapabilityValidator {
    let sessionless = book.ingest(registry, WriteAdmission::Nothing);
    Arc::new(move |entry, _from| sessionless(entry))
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
        let book = AccessBook::new(pdn_types::PdnId::from_bytes([1u8; 32]));
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
        let book = AccessBook::new(pdn_types::PdnId::from_bytes([1u8; 32]));
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
        let book = AccessBook::new(pdn_types::PdnId::from_bytes([1u8; 32]));
        poison(&book.retractions);
        assert!(
            book.retraction_names(namespace.id(), &signed(&namespace, &author, "devices/x", 1)),
            "unreadable: claiming the match is the fail-closed side"
        );

        let registry = Arc::new(Registry::default());
        let verdict = book.ingest(&registry, WriteAdmission::Nothing);
        assert_eq!(
            verdict(&signed(&namespace, &author, "devices/x", 1)),
            ValidateOutcome::Accept
        );
    }
}
