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
use pdn_store::{api::Doc, store::Query, EntryFilter, NamespaceId, SessionAccess, SessionRole};
use pdn_types::{ClaimId, NodeId, PdnId};

use crate::connection_metadata::GrantRecord;
use crate::grant::claim_id_of_key;
use crate::node::read_payload;
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

/// The width of what one connection grants on one issuer's data.
enum GrantWidth {
    /// A scoped grant: exactly these claims.
    Claims(Vec<ClaimId>),
    /// A whole-store grant (a ticket record, no capability).
    WholeStore,
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
        // The issuer's own devices see everything, judged through the
        // hosted directory.
        if let Some(directory) = self.directory_of(issuer)? {
            if device_listed(&directory, caller).await? {
                return Ok(SessionAccess::Full);
            }
            // Granted counterparties: the union of the grants every matching
            // connection carries for this caller (a device published by two
            // hosted identities gets the union). Width is read only from a
            // present, decoded record — a whole-store record classifies as
            // full. Fail-closed for callers with no grant at all, and for
            // records still replicating or undecodable by this build.
            let mut claims: HashSet<ClaimId> = HashSet::new();
            let mut whole_store = false;
            for connection in self.connections_of_identity(issuer)? {
                if !device_listed(&connection.peer_doc, caller).await? {
                    continue;
                }
                match self.granted_claims(&connection, issuer).await? {
                    GrantWidth::Claims(grant_claims) => claims.extend(grant_claims),
                    GrantWidth::WholeStore => whole_store = true,
                    GrantWidth::None => {}
                }
            }
            if whole_store {
                return Ok(SessionAccess::Full);
            }
            if claims.is_empty() {
                return Ok(SessionAccess::Deny);
            }
            return Ok(SessionAccess::Filtered(egress_filter(issuer, claims)));
        }

        // This node does not host the issuer. A grantee binding (`Never`)
        // still recognizes the issuer's own devices (via their published
        // device set) and gives them the full view; every other caller is
        // refused, uniform with not-hosted — a grantee never re-serves its
        // slice. Dialing out keeps a closed egress: serve nothing, receive
        // whatever the callee's own filter admits. A `Serve` binding is the
        // ticket-bounded stance: the whole replica.
        match posture {
            ServingPosture::Never => {
                for connection in self.connections_with_peer(issuer)? {
                    if device_listed(&connection.peer_doc, caller).await? {
                        return Ok(SessionAccess::Full);
                    }
                }
                Ok(match role {
                    SessionRole::Accept => SessionAccess::Deny,
                    SessionRole::Dial => SessionAccess::Filtered(closed_egress()),
                })
            }
            ServingPosture::Serve => Ok(SessionAccess::Full),
        }
    }

    /// What the connection's `own` store grants its peer on `issuer`'s
    /// data, read payload-waiting from the one grant record. The width
    /// comes only from a present, *decoded* record: a scoped record yields
    /// its capability's exact claims, a whole-store record the whole
    /// replica; everything else — no record, a payload still replicating, a
    /// record kind this build cannot decode — is no grant. Width must never
    /// be inferred from a record's mere presence, or an unreadable narrow
    /// grant would classify as a wide one.
    async fn granted_claims(
        &self,
        connection: &HostedConnection,
        issuer: PdnId,
    ) -> Result<GrantWidth> {
        let Some(blobs) = self.blobs.get() else {
            return Ok(GrantWidth::None);
        };
        let key = crate::connection_metadata::grant_key(&issuer);
        let Some(bytes) = read_payload(&connection.own, blobs, key.as_bytes()).await? else {
            return Ok(GrantWidth::None);
        };
        Ok(
            match crate::connection_metadata::decode_grant_record(&bytes) {
                Some(GrantRecord::Scoped { cap, .. }) => GrantWidth::Claims(cap.claims.into_vec()),
                Some(GrantRecord::WholeStore { .. }) => GrantWidth::WholeStore,
                None => GrantWidth::None,
            },
        )
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

/// Whether `device` is recorded (record-level, tombstones excluded) under
/// the `devices/` prefix of `doc` — probed through the one shared key
/// definition: this membership test decides "own device", so it must never
/// drift from what the stores write.
async fn device_listed(doc: &Doc, device: NodeId) -> Result<bool> {
    let key = crate::private_metadata::device_key(&device);
    let query = Query::single_latest_per_key().key_exact(key.as_bytes());
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
