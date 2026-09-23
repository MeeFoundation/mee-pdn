//! The connection metadata store: one dedicated replica per direction of a
//! connection, written only by the issuing identity's devices and read
//! whole by the counterparty's (Invariant 3). Grants ride inside as one
//! record per granted data store at `grants/<issuer-hex>`: a publish
//! replaces it wholesale, a withdrawal is one tombstone. Grant payloads are
//! blobs, so a read is three-valued ([`GrantRead`]): a record whose payload
//! has not arrived, or whose bytes this build cannot read, is neither a
//! grant nor a decided absence — and it withholds only itself, never the
//! readable grants beside it, since the counterparty is what writes here.

use anyhow::Result;
use futures_core::Stream;
use futures_lite::StreamExt;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    engine::LiveEvent,
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use pdn_types::{NodeId, PdnId};
use serde::{Deserialize, Serialize};

use crate::{
    grant::ReadGrant,
    node::SyncNode,
    private_metadata::{device_key, device_of, DEVICES_PREFIX},
};

/// Key prefix under which grant entries live.
const GRANTS_PREFIX: &str = "grants/";

/// `grants/<issuer-hex>` — shared with the access book, which reads the
/// same record at session classification.
pub(crate) fn grant_key(issuer: &PdnId) -> String {
    format!("{GRANTS_PREFIX}{issuer}")
}

/// The payload at `grants/<issuer-hex>`, tagged JSON: the tag is the
/// structural version, so a kind this build does not know fails to decode
/// and reads as no grant (fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum GrantRecord {
    Scoped {
        cap: ReadGrant,
        /// Canonical string form; its `ShareMode` follows the grant's commands.
        ticket: String,
    },
}

/// What one metadata replica says about the grant of one issuer's data
/// toward one audience. Three answers, not two: an absent record and a
/// record whose payload has not replicated are opposite facts for a caller
/// deciding whether it may write.
#[derive(Debug, Clone)]
pub enum GrantRead {
    /// Boxed: the ticket dwarfs the two empty variants beside it.
    Granted(ReadGrant, Box<DocTicket>),
    /// No record at all (never published, or withdrawn), or a record whose
    /// capability grants someone else.
    None,
    /// A record whose payload has not replicated, or whose bytes this build
    /// cannot read. Consumers poll; nothing may be concluded either way.
    Unreadable,
}

impl GrantRead {
    pub fn granted(self) -> Option<(ReadGrant, DocTicket)> {
        match self {
            GrantRead::Granted(cap, ticket) => Some((cap, *ticket)),
            GrantRead::None | GrantRead::Unreadable => None,
        }
    }
}

/// `None` for unreadable bytes, never an error: one unreadable grant
/// withholds itself and never the readable grants beside it.
pub(crate) fn decode_grant_record(bytes: &[u8]) -> Option<GrantRecord> {
    serde_json::from_slice(bytes).ok()
}

fn grant_issuer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(GRANTS_PREFIX)?
        .parse()
        .ok()
}

/// Same withholds-itself-only rule as [`decode_grant_record`].
fn decode_grant_ticket(ticket: &str) -> Option<DocTicket> {
    ticket.parse().ok()
}

/// The directory kind of the write ticket to the identity's own metadata
/// store toward `peer` — how the issuer's other devices open `own`.
pub fn own_ticket_kind(peer: &PdnId) -> String {
    format!("connection-metadata/{peer}/own")
}

/// The directory kind of the read ticket to the counterpart's metadata
/// store received from `peer` — how the identity's other devices open
/// `peer`.
pub fn peer_ticket_kind(peer: &PdnId) -> String {
    format!("connection-metadata/{peer}/peer")
}

/// One direction of a connection's metadata channel. The issuing identity
/// and the counterparty are not kept here — the handle's identity knows which
/// connection and direction it serves.
#[derive(Debug, Clone)]
pub struct ConnectionMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
}

/// The metadata pair at one side of a connection. The same replica is
/// `own` at its issuer and `peer` at the counterparty.
#[derive(Debug, Clone)]
pub struct ConnectionMetadata {
    /// The replica this side issues and writes grants into.
    pub own: ConnectionMetadataStore,
    /// The counterpart's replica, imported from the read ticket received at
    /// establishment.
    pub peer: ConnectionMetadataStore,
}

impl ConnectionMetadataStore {
    /// Create a fresh metadata store held for `identity` — this side's
    /// `own` replica toward one counterparty.
    pub async fn create(node: &SyncNode, identity: PdnId) -> Result<Self> {
        // Author first, tracked doc last: nothing awaits between the
        // tracking and the handle reaching the caller, so a dropped future
        // cannot leave a tracked replica no handle refers to. The
        // identity's one author: per-store authors would leave a record
        // written before a restart standing beside its replacement written
        // after one.
        let author = node.default_author(identity)?;
        let doc = node.new_doc(identity).await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Import a metadata store via `ticket` — the counterpart's replica from
    /// the read ticket, or this identity's own replica from the write ticket
    /// in the directory. Usable at once; content converges asynchronously.
    pub async fn import(node: &SyncNode, identity: PdnId, ticket: DocTicket) -> Result<Self> {
        // Author first, tracked doc last — see `create`.
        let author = node.default_author(identity)?;
        let doc = node.import_doc(identity, ticket).await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Lets a re-established peer store (same namespace) be told from a new
    /// one, so re-establishment does not leak a tracked doc per attempt.
    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
    }

    /// For registration with the node's access book.
    pub(crate) fn doc_handle(&self) -> Doc {
        self.doc.clone()
    }

    /// One item per observed change of this replica — an entry written here,
    /// an entry arrived by sync, or a payload blob become readable. Detail-free
    /// on purpose: the fork's event vocabulary stays behind this layer.
    /// `ContentReady` counts because grant payloads are blobs: only the
    /// payload event tells a consumer a record has become readable. An `Err`
    /// item is the subscription failing; the stream ends with the node.
    pub async fn changes(&self) -> Result<impl Stream<Item = Result<()>> + Send + Unpin + 'static> {
        let events = self.doc.subscribe().await?;
        Ok(events.filter_map(|event| match event {
            Ok(
                LiveEvent::InsertLocal { .. }
                | LiveEvent::InsertRemote { .. }
                | LiveEvent::ContentReady { .. },
            ) => Some(Ok(())),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }))
    }

    /// `ShareMode::Read` for the counterparty (inside the establishment
    /// dialogue), `ShareMode::Write` for the issuer's own directory.
    pub async fn share_ticket(
        &self,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// The issuers with a live grant, record-level: a listed grant may still
    /// read as unreadable in [`read_grant`](Self::read_grant).
    pub async fn list_grants(&self) -> Result<Vec<PdnId>> {
        let query = Query::single_latest_per_key().key_prefix(GRANTS_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut issuers = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(issuer) = grant_issuer_of(entry?.key()) {
                issuers.push(issuer);
            }
        }
        Ok(issuers)
    }

    /// One tombstone over the one record: no state in which a classifier
    /// reads a half-withdrawn grant.
    pub async fn withdraw_grant(&self, issuer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, grant_key(&issuer).into_bytes())
            .await?;
        Ok(())
    }

    /// One write carrying capability and ticket together, so neither can
    /// exist without the other in any order of replication. The ticket's
    /// mode is the caller's to mint per the grant's commands; this store
    /// carries the pair, it does not check it.
    pub async fn publish_grant(&self, grant: &ReadGrant, ticket: &DocTicket) -> Result<()> {
        let record = GrantRecord::Scoped {
            cap: grant.clone(),
            ticket: ticket.to_string(),
        };
        self.doc
            .set_bytes(
                self.author,
                grant_key(&grant.issuer).into_bytes(),
                serde_json::to_vec(&record)?,
            )
            .await?;
        Ok(())
    }

    /// Read the grant for `issuer`'s data store toward `audience`. Position
    /// says who wrote a record; only the capability says over whose data and
    /// toward whom, and the key is the counterparty's word too — so a record
    /// naming another issuer or audience is no grant here. The access book
    /// applies the same test at classification. `Err` is reserved for this
    /// node's own failures.
    pub async fn read_grant(&self, issuer: PdnId, audience: PdnId) -> Result<GrantRead> {
        let key = grant_key(&issuer);
        let query = Query::single_latest_per_key().key_exact(key.as_bytes());
        // A withdrawal is a tombstone this query skips: "no entry" and
        // "withdrawn" are the same decided absence.
        let Some(entry) = self.doc.get_one(query).await? else {
            return Ok(GrantRead::None);
        };
        let hash = entry.content_hash();
        if !self.blobs.has(hash).await? {
            return Ok(GrantRead::Unreadable);
        }
        let bytes = self.blobs.get_bytes(hash).await?;
        match decode_grant_record(&bytes) {
            Some(GrantRecord::Scoped { cap, ticket })
                if cap.issuer == issuer && cap.audience == audience =>
            {
                Ok(match decode_grant_ticket(&ticket) {
                    Some(ticket) => GrantRead::Granted(cap, Box::new(ticket)),
                    None => GrantRead::Unreadable,
                })
            }
            // Grants someone else: a decided answer.
            Some(GrantRecord::Scoped { .. }) => Ok(GrantRead::None),
            // What unknown bytes grant is unknown, not nothing.
            None => Ok(GrantRead::Unreadable),
        }
    }

    /// The deliberate (re-)assertion act: writes the record whatever the set
    /// holds, a withdrawn one included. Machinery that merely opens the pair
    /// uses [`ensure_device_published`](Self::ensure_device_published).
    pub async fn publish_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .set_bytes(self.author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Publish `device` only if the set carries no record of it at all, live
    /// or withdrawn: re-signing a live record would refresh its timestamp,
    /// and re-asserting a withdrawn one would out-bid the tombstone by wall
    /// clock on every pair opening.
    pub async fn ensure_device_published(&self, device: NodeId) -> Result<()> {
        // `include_empty` keeps tombstones visible.
        let query = Query::single_latest_per_key()
            .key_exact(device_key(&device).into_bytes())
            .include_empty();
        if self.doc.get_one(query).await?.is_some() {
            return Ok(());
        }
        self.publish_device(device).await
    }

    /// The published devices, record-level. Only keys that resolve into
    /// endpoint ids leave here: the key is the counterparty's word, and a
    /// garbage record withholds itself rather than costing every consumer of
    /// the set a conversion failure.
    pub async fn published_devices(&self) -> Result<Vec<NodeId>> {
        let query = Query::single_latest_per_key().key_prefix(DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut devices = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(device) = device_of(entry?.key()) {
                if iroh::EndpointId::from_bytes(device.as_bytes()).is_ok() {
                    devices.push(device);
                } else {
                    // Debug, not warn: counterparty-written and re-read
                    // every sweep, so a warn would fill logs at alarm level.
                    tracing::debug!(%device, "withheld an unresolvable device record");
                }
            }
        }
        Ok(devices)
    }

    /// Tombstone a published device record.
    pub async fn withdraw_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .del(self.author, device_key(&device).into_bytes())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use iroh::{EndpointAddr, PublicKey};
    use pdn_store::{Capability, NamespaceSecret};
    use pdn_types::{ClaimId, NonEmpty};

    use super::*;
    use crate::grant::GrantedClaim;

    fn ticket() -> DocTicket {
        // A ticket without addressing does not decode at all.
        let node =
            PublicKey::from_str("ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6")
                .expect("valid test key");
        DocTicket::new(
            Capability::Write(NamespaceSecret::from_bytes(&[7u8; 32])),
            vec![EndpointAddr::new(node)],
            crate::identity_of(pdn_types::PdnId::from_bytes([3u8; 32])),
        )
    }

    /// Unreadable bytes read as no grant, never as an error; the real record
    /// kind decodes, so "absent" is a verdict on the bytes and not a decoder
    /// that never says yes.
    #[test]
    fn grant_records_decode_and_unreadable_ones_read_as_absent() {
        let unknown_kind = br#"{"kind":"from_a_later_version","ticket":"x"}"#;
        assert!(
            decode_grant_record(unknown_kind).is_none(),
            "a record kind this version does not know must read as absent"
        );

        let issuer = PdnId::from_bytes([0xa1; 32]);
        let scoped = serde_json::to_vec(&GrantRecord::Scoped {
            cap: ReadGrant {
                issuer,
                audience: PdnId::from_bytes([0xb0; 32]),
                claims: NonEmpty::new(GrantedClaim {
                    claim: ClaimId::from_bytes([0x11; 32]),
                    write: false,
                }),
            },
            ticket: ticket().to_string(),
        })
        .expect("serializable");
        assert!(
            matches!(
                decode_grant_record(&scoped),
                Some(GrantRecord::Scoped { .. })
            ),
            "a scoped record must decode"
        );

        // A flat claim list with one grant-wide write flag reads as no
        // grant — fail-closed, never a default width.
        let flat_cap_record = format!(
            r#"{{"kind":"scoped","cap":{{"issuer":"{issuer}","audience":"{audience}","claims":["{claim}"],"write":true}},"ticket":"{ticket}"}}"#,
            audience = PdnId::from_bytes([0xb0; 32]),
            claim = ClaimId::from_bytes([0x11; 32]),
            ticket = ticket(),
        );
        assert!(
            decode_grant_record(flat_cap_record.as_bytes()).is_none(),
            "a flat-claims capability payload must read as absent"
        );

        assert!(decode_grant_record(&[0xff, 0xfe]).is_none(), "not utf-8");
        assert!(
            decode_grant_record(b"not a record").is_none(),
            "utf-8 but not a record"
        );
        assert!(decode_grant_record(b"").is_none(), "empty payload");
        assert!(
            decode_grant_record(br#"{"kind":"delegated_chain","token":"opaque"}"#).is_none(),
            "an unknown record kind must read as absent"
        );
        assert!(
            decode_grant_ticket("not a ticket").is_none(),
            "an unreadable ticket string must read as absent"
        );
    }
}
