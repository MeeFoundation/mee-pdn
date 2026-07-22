//! The connection metadata store: the cross-identity channel of a
//! connection — one dedicated replica per direction. The store issued by
//! identity A toward its counterparty B carries A's grants to B: written
//! only by A's devices, read whole by B's devices, and to every other party
//! not observably in existence (Invariant 3). The counterparty is the
//! audience of the entire replica, so no per-record filtering applies
//! inside it.
//!
//! At each side of a connection the pair is held as [`ConnectionMetadata`]:
//! `own` — the replica this side issues and writes — and `peer` — the
//! counterpart's, imported from the read ticket received at establishment.
//! The same replica is `own` at its issuer and `peer` at the counterparty.
//!
//! Grants ride inside as **one record per granted data store**, at
//! `grants/<issuer-hex>` — a single entry carrying the capability that
//! scopes it to an exact claim set ([`GrantRecord`]). At every moment
//! exactly one grant exists per issuer: every publish replaces it
//! wholesale, and a withdrawal is one tombstone — no ordering between
//! records to get wrong, locally or across devices.
//!
//! Grant payloads are blobs, so grant reads are payload-waiting:
//! [`ConnectionMetadataStore::read_grant`] returns `None` until the payload
//! bytes have arrived — and likewise for bytes this version cannot read,
//! since the counterparty is what writes them here and one unreadable grant
//! must not hide the readable ones beside it. The serving side derives a
//! caller's rights only from a present, decoded record: absence, a lagging
//! payload, and an undecodable payload all classify as *no grant*.

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

use crate::grant::ReadGrant;
use crate::node::{read_payload, SyncNode};
use crate::private_metadata::{device_key, device_of, DEVICES_PREFIX};

/// Key prefix under which grant entries live.
const GRANTS_PREFIX: &str = "grants/";

/// The entry key of the grant record for `issuer`'s data store:
/// `grants/<issuer-hex>` — one record per issuer. Shared with the access
/// book, which reads the same record at session classification.
pub(crate) fn grant_key(issuer: &PdnId) -> String {
    format!("{GRANTS_PREFIX}{issuer}")
}

/// The one grant record of one data store's issuer — the payload at
/// `grants/<issuer-hex>`. Serialized as tagged JSON: the tag is the
/// structural version, so a record kind this build does not know fails to
/// decode and reads as *no grant* (fail-closed) rather than as something
/// this build would act on. The ticket travels in its canonical string
/// form; its `ShareMode` follows the grant's commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum GrantRecord {
    /// A capability-scoped grant: exactly the capability's claims, with the
    /// ticket carrying addressing and contacts. The one kind there is —
    /// the tagged form stays so an unknown kind from a future version
    /// decodes as no grant rather than as this one.
    Scoped {
        /// The capability: issuer, audience, exact claims, commands.
        cap: ReadGrant,
        /// The replica's ticket, canonical string form.
        ticket: String,
    },
}

/// Decode a grant record's payload, if it is one this version can read.
/// `None` for unreadable bytes: the counterparty owns this store's
/// contents, so an unreadable payload withholds that one grant exactly as
/// writing nothing would — treating it as an error would let a single
/// unreadable grant hide every readable grant beside it. The serving side
/// leans on the same `None`: undecodable never classifies wider than
/// absent.
pub(crate) fn decode_grant_record(bytes: &[u8]) -> Option<GrantRecord> {
    serde_json::from_slice(bytes).ok()
}

/// Parse a data-store issuer back out of a `grants/<hex>` key, if it
/// matches.
fn grant_issuer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(GRANTS_PREFIX)?
        .parse()
        .ok()
}

/// Parse a grant record's ticket string back into a [`DocTicket`], `None`
/// for a form this version cannot read — the same withholds-itself-only
/// rule as [`decode_grant_record`].
fn decode_grant_ticket(ticket: &str) -> Option<DocTicket> {
    ticket.parse().ok()
}

/// The private-metadata directory kind under which establishment publishes
/// the write ticket to the identity's own metadata store toward `peer` —
/// how the issuer's other devices open `own` for writing.
pub fn own_ticket_kind(peer: &PdnId) -> String {
    format!("connection-metadata/{peer}/own")
}

/// The private-metadata directory kind under which establishment publishes
/// the read ticket to the counterpart's metadata store received from `peer`
/// — how the identity's other devices open `peer` for reading.
pub fn peer_ticket_kind(peer: &PdnId) -> String {
    format!("connection-metadata/{peer}/peer")
}

/// One direction of a connection's metadata channel: a dedicated replica
/// written by its issuing identity's devices and read whole by the
/// connection counterparty's devices (Invariant 3). The issuing identity
/// and the counterparty are not kept here — the handle's holder knows which
/// connection and direction it serves.
#[derive(Debug, Clone)]
pub struct ConnectionMetadataStore {
    doc: Doc,
    author: AuthorId,
    blobs: iroh_blobs::api::Store,
}

/// The metadata pair at one side of a connection: `own` — the replica this
/// side issues and writes grants into — and `peer` — the counterpart's
/// replica, imported from the read ticket received at establishment. The
/// same replica is `own` at its issuer and `peer` at the counterparty.
#[derive(Debug, Clone)]
pub struct ConnectionMetadata {
    /// The replica this side issues toward the counterparty.
    pub own: ConnectionMetadataStore,
    /// The counterpart's replica, imported read-only.
    pub peer: ConnectionMetadataStore,
}

impl ConnectionMetadataStore {
    /// Create a fresh metadata store on `node` — this side's `own` replica
    /// toward one counterparty.
    pub async fn create(node: &SyncNode) -> Result<Self> {
        let doc = node.new_doc().await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// Import a metadata store via `ticket`: the counterpart's replica from
    /// the read ticket received at establishment, or this identity's own
    /// replica from the write ticket in the directory. The handle is usable
    /// at once; content converges asynchronously.
    pub async fn import(node: &SyncNode, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_doc(ticket).await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// The namespace id of the backing replica. Lets a re-established peer
    /// store (same namespace) be told from a genuinely new one, so
    /// re-establishment does not re-import and leak a tracked doc plus an
    /// author per attempt.
    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
    }

    /// The backing doc handle, for registration with the node's access
    /// book ([`SyncNode::host_connection`](crate::SyncNode::host_connection)).
    pub(crate) fn doc_handle(&self) -> Doc {
        self.doc.clone()
    }

    /// One item per observed change of this replica: an entry written here,
    /// an entry arrived by sync, or a payload blob become readable. The item
    /// carries no detail on purpose — the fork's event vocabulary stays
    /// behind this layer, and "something changed, look again" is what a
    /// re-reading consumer needs. `ContentReady` is one of the three because
    /// grant payloads are blobs: a grant record whose entry arrived is not
    /// yet readable, and only the payload event tells a consumer to re-read.
    /// An `Err` item reports the subscription failing; the stream ends when
    /// the node shuts down.
    pub async fn changes(&self) -> Result<impl Stream<Item = Result<()>> + Send + Unpin + 'static> {
        let events = self.doc.subscribe().await?;
        Ok(events.filter_map(|event| match event {
            Ok(
                LiveEvent::InsertLocal { .. }
                | LiveEvent::InsertRemote { .. }
                | LiveEvent::ContentReady { .. },
            ) => Some(Ok(())),
            // Sync-session and neighbor bookkeeping is not a change of the
            // replica's contents.
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }))
    }

    /// Share this store as a ticket: `ShareMode::Read` for the counterparty
    /// (handed over inside the establishment dialogue), `ShareMode::Write`
    /// for the issuing identity's own directory (every device of the issuer
    /// writes grants).
    pub async fn share_ticket(
        &self,
        mode: ShareMode,
        addr_options: AddrInfoOptions,
    ) -> Result<DocTicket> {
        let ticket = self.doc.share(mode, addr_options).await?;
        Ok(ticket)
    }

    /// List the data-store issuers with a live grant (record-level —
    /// available as soon as the records sync; a listed grant's ticket may
    /// still be payload-waiting in [`read_grant`](Self::read_grant)).
    /// Withdrawn grants do not list.
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

    /// Withdraw the grant for `issuer`'s data store — one tombstone (empty
    /// entry) over the one record, replicating like
    /// any other entry: the counterparty eventually reads the grant as
    /// absent, and the issuer's own devices stop admitting the audience at
    /// the next session setup (rights are frozen per session). One record
    /// means one tombstone: no state in which a classifier could read a
    /// half-withdrawn grant. Whether data already delivered under it
    /// is retained is outside this store — Invariant 2 governs acquisition,
    /// not retention.
    pub async fn withdraw_grant(&self, issuer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, grant_key(&issuer).into_bytes())
            .await?;
        Ok(())
    }

    /// Publish a grant: one [`GrantRecord::Scoped`] carrying the capability
    /// and its ticket at `grants/<issuer-hex>` — a single write, so the
    /// capability and the ticket cannot exist without each other in any
    /// order of replication. The ticket's mode is the caller's to mint per
    /// the grant's commands — read-only → `ShareMode::Read`, with write →
    /// `ShareMode::Write`; this store carries the pair, it does not check
    /// it.
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

    /// Read the grant for `issuer`'s data store — the capability and its
    /// ticket — if present and readable.
    ///
    /// `Ok(None)` covers every "no usable grant here": no entry at all, a
    /// payload that has not arrived (consumers poll), or a payload this
    /// version cannot read. `Err` stays reserved for this node's own
    /// failures, so one unreadable grant never hides the readable ones
    /// beside it.
    pub async fn read_grant(&self, issuer: PdnId) -> Result<Option<(ReadGrant, DocTicket)>> {
        let Some(bytes) =
            read_payload(&self.doc, &self.blobs, grant_key(&issuer).as_bytes()).await?
        else {
            return Ok(None);
        };
        match decode_grant_record(&bytes) {
            Some(GrantRecord::Scoped { cap, ticket }) => {
                Ok(decode_grant_ticket(&ticket).map(|t| (cap, t)))
            }
            None => Ok(None),
        }
    }

    /// Publish `device` as one of the issuing identity's devices: the
    /// record the counterparty resolves a caller's authenticated node id
    /// through. Unconditional: writes the record whatever the set holds, a
    /// withdrawn record included — the deliberate (re-)assertion act.
    /// Machinery that merely opens the pair uses
    /// [`ensure_device_published`](Self::ensure_device_published) instead.
    pub async fn publish_device(&self, device: NodeId) -> Result<()> {
        self.doc
            .set_bytes(self.author, device_key(&device).into_bytes(), vec![1u8])
            .await?;
        Ok(())
    }

    /// Publish `device` only if the set carries no record of it at all —
    /// live *or withdrawn*. A live record makes this a no-op (re-signing
    /// would only refresh its timestamp); so does a tombstone: a withdrawn
    /// device must never be re-asserted as a side effect of merely opening
    /// the pair — re-assertion is the deliberate
    /// [`publish_device`](Self::publish_device).
    pub async fn ensure_device_published(&self, device: NodeId) -> Result<()> {
        // `include_empty` keeps tombstones visible: "no record at all" and
        // "withdrawn" must be told apart, and only the former publishes.
        let query = Query::single_latest_per_key()
            .key_exact(device_key(&device).into_bytes())
            .include_empty();
        if self.doc.get_one(query).await?.is_some() {
            return Ok(());
        }
        self.publish_device(device).await
    }

    /// The devices the issuing identity has published (record-level —
    /// available as soon as the records sync).
    pub async fn published_devices(&self) -> Result<Vec<NodeId>> {
        let query = Query::single_latest_per_key().key_prefix(DEVICES_PREFIX.as_bytes());
        let mut stream = std::pin::pin!(self.doc.get_many(query).await?);
        let mut devices = Vec::new();
        while let Some(entry) = stream.next().await {
            if let Some(device) = device_of(entry?.key()) {
                devices.push(device);
            }
        }
        Ok(devices)
    }

    /// Revoke a published device record (tombstone) — a revoked device
    /// stops classifying as this identity's on the counterparty.
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

    fn ticket() -> DocTicket {
        // A ticket carries addressing info — a ticket without it does not
        // decode at all — so the readable cases need a node in it.
        let node =
            PublicKey::from_str("ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6")
                .expect("valid test key");
        DocTicket::new(
            Capability::Write(NamespaceSecret::from_bytes(&[7u8; 32])),
            vec![EndpointAddr::new(node)],
        )
    }

    /// A grant payload this version cannot read is absent, not an error —
    /// it withholds only itself, never the readable grants beside it, and
    /// never classifies wider than absent. The real record kind decodes, so
    /// "absent" is a verdict on the bytes and not a decoder that never says
    /// yes; a tagged kind this version does not know reads as absent too,
    /// which is what keeps a future width from classifying as this one.
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
                claims: NonEmpty::new(ClaimId::from_bytes([0x11; 32])),
                write: false,
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

        assert!(decode_grant_record(&[0xff, 0xfe]).is_none(), "not utf-8");
        assert!(
            decode_grant_record(b"not a record").is_none(),
            "utf-8 but not a record"
        );
        assert!(decode_grant_record(b"").is_none(), "empty payload");
        // The tag is the structural version: an unknown record kind reads
        // as absent — fail-closed, never a default width.
        assert!(
            decode_grant_record(br#"{"kind":"delegated_chain","token":"opaque"}"#).is_none(),
            "an unknown record kind must read as absent"
        );
        // A ticket string this version cannot parse: the record decodes,
        // the ticket does not — the read surfaces treat that one grant as
        // absent.
        assert!(
            decode_grant_ticket("not a ticket").is_none(),
            "an unreadable ticket string must read as absent"
        );
    }
}
