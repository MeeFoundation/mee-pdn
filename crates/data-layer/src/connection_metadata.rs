//! The connection metadata store: the cross-identity channel of a
//! connection — one dedicated replica per direction.
//!
//! For each connection there are two of these stores. The store issued by
//! identity A toward its counterparty B carries A's grants to B: written
//! only by A's devices, read whole by B's devices, and to every other party
//! not observably in existence (Invariant 3 —
//! `mia-docs/openspec/specs/components/pdn-node/invariants.md`). The
//! counterparty is the audience of the entire replica, so no per-record
//! filtering applies inside it. The mechanism is the one the other device
//! stores already use: a dedicated pdn-store replica gated by ticket
//! possession — no new sync machinery, and no domain `NamespaceId`.
//!
//! At each side of a connection the pair is held as [`ConnectionMetadata`]:
//! `own` — the replica this side issues and writes — and `peer` — the
//! counterpart's, imported from the read ticket received at establishment.
//! The same replica is `own` at its issuer and `peer` at the counterparty.
//! Importing binds the local replica to the issuing namespace immediately;
//! content converges asynchronously.
//!
//! Grants ride inside, keyed by the granted data store's issuer: the
//! whole-store ticket at `grants/<issuer-hex>/ticket` (the interim payload),
//! the read capability at `grants/<issuer-hex>/cap` — a slot reserved and
//! unwritten until the read-capability mechanism lands (subset-rbsr).
//! Capability payloads stay opaque bytes at this layer.
//!
//! Ticket payloads are blobs, so grant reads are payload-waiting:
//! [`ConnectionMetadataStore::read_grant`] returns `None` until the payload
//! bytes have arrived, exactly like the directory's ticket reads — and
//! likewise for bytes this version cannot read, since the counterparty is
//! what writes them here and one unreadable grant must not hide the
//! readable ones beside it.

use anyhow::Result;
use futures_lite::StreamExt;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use pdn_types::PdnId;

use crate::node::{read_payload, SyncNode};

/// Key prefix under which grant entries live.
const GRANTS_PREFIX: &str = "grants/";
/// Key suffix of a grant's ticket slot. The sibling `/cap` slot is reserved
/// for the read capability and stays unwritten until subset-rbsr.
const TICKET_SUFFIX: &str = "/ticket";

/// The entry key of the ticket slot of the grant for `issuer`'s data store:
/// `grants/<issuer-hex>/ticket`.
fn grant_ticket_key(issuer: &PdnId) -> String {
    format!("{GRANTS_PREFIX}{issuer}{TICKET_SUFFIX}")
}

/// Parse a data-store issuer back out of a `grants/<hex>/ticket` key, if it
/// matches.
fn grant_issuer_of(key: &[u8]) -> Option<PdnId> {
    std::str::from_utf8(key)
        .ok()?
        .strip_prefix(GRANTS_PREFIX)?
        .strip_suffix(TICKET_SUFFIX)?
        .parse()
        .ok()
}

/// Decode a grant's payload into the ticket it carries, if it is one.
///
/// `None` for bytes this version cannot read — not UTF-8, or not a ticket.
/// The counterparty owns this store's contents, so an unreadable payload is
/// its doing (a newer ticket form, a buggy writer) and withholds that one
/// grant exactly as writing nothing would; it is not this node's error, and
/// treating it as one would let a single unreadable grant hide every
/// readable grant beside it. Contrast the directory's
/// [`PrivateMetadataStore::get_ticket`](crate::PrivateMetadataStore::get_ticket),
/// whose only writers are the identity's own devices — garbage there is our
/// own bug and stays an error.
fn decode_grant_ticket(bytes: &[u8]) -> Option<DocTicket> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
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
/// connection counterparty's devices (Invariant 3).
///
/// Built from a [`SyncNode`]; holds the backing replica, an author for
/// local writes, and the blob store for payload-waiting grant reads. The
/// issuing identity and the counterparty are not kept here — the handle's
/// holder knows which connection and direction it serves, and one node
/// holds the metadata stores of any number of connections and identities.
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
    /// toward one counterparty, created once per connection direction and
    /// thereafter reused (looked up through the directory).
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
    /// replica from the write ticket in the directory (a linked device
    /// opening the pair). Binds to the issuing namespace immediately — the
    /// handle is usable at once, content converges asynchronously.
    pub async fn import(node: &SyncNode, ticket: DocTicket) -> Result<Self> {
        let doc = node.import_doc(ticket).await?;
        let author = node.create_author().await?;
        Ok(Self {
            doc,
            author,
            blobs: node.blobs(),
        })
    }

    /// The namespace id of the backing replica — which replica this handle
    /// addresses. Lets a re-established peer store (same namespace, reuse)
    /// be told from a genuinely new one (import), so re-establishment does
    /// not re-import and leak a tracked doc plus an author per attempt.
    pub fn namespace(&self) -> NamespaceId {
        self.doc.id()
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

    /// Publish a grant of `issuer`'s data store: the whole-store `ticket` at
    /// `grants/<issuer-hex>/ticket` — the interim payload. The `/cap` slot
    /// next to it stays reserved and unwritten until the read-capability
    /// mechanism lands.
    pub async fn publish_grant(&self, issuer: PdnId, ticket: &DocTicket) -> Result<()> {
        self.doc
            .set_bytes(
                self.author,
                grant_ticket_key(&issuer).into_bytes(),
                ticket.to_string().into_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Read the grant for `issuer`'s data store, if present and readable.
    ///
    /// `Ok(None)` covers every "no usable grant here": no entry at all; the
    /// entry's payload has not arrived — records and payloads travel
    /// independently, so consumers poll, as they do for the directory's
    /// tickets; or the payload is not a ticket this version can read
    /// ([`decode_grant_ticket`]). `Err` stays reserved for this node's own
    /// failures — the replica and blob reads — so one unreadable grant never
    /// hides the readable ones beside it.
    pub async fn read_grant(&self, issuer: PdnId) -> Result<Option<DocTicket>> {
        let Some(bytes) =
            read_payload(&self.doc, &self.blobs, grant_ticket_key(&issuer).as_bytes()).await?
        else {
            return Ok(None);
        };
        Ok(decode_grant_ticket(&bytes))
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

    /// Withdraw the grant for `issuer`'s data store — writes a tombstone
    /// (empty entry) that replicates like any other entry, so the
    /// counterparty eventually reads the grant as absent. Whether data
    /// already delivered under the grant is retained is outside this store —
    /// Invariant 2 governs acquisition, not retention.
    pub async fn withdraw_grant(&self, issuer: PdnId) -> Result<()> {
        self.doc
            .del(self.author, grant_ticket_key(&issuer).into_bytes())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use iroh::{EndpointAddr, PublicKey};
    use pdn_store::{Capability, NamespaceSecret};

    use super::*;

    /// A grant payload the counterparty wrote that this version cannot read
    /// is absent, not an error — so it withholds only itself, never the
    /// readable grants beside it in the same store. A real ticket still
    /// decodes, so "absent" is a verdict on those bytes and not a decoder
    /// that never says yes.
    #[test]
    fn grant_payloads_decode_and_unreadable_ones_read_as_absent() {
        // A ticket carries addressing info — a ticket without it does not
        // decode at all — so the readable case needs a node in it.
        let node =
            PublicKey::from_str("ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6")
                .expect("valid test key");
        let ticket = DocTicket::new(
            Capability::Write(NamespaceSecret::from_bytes(&[7u8; 32])),
            vec![EndpointAddr::new(node)],
        );
        assert!(
            decode_grant_ticket(ticket.to_string().as_bytes()).is_some(),
            "a real ticket must decode"
        );

        assert!(decode_grant_ticket(&[0xff, 0xfe]).is_none(), "not utf-8");
        assert!(
            decode_grant_ticket(b"not a ticket").is_none(),
            "utf-8 but not a ticket"
        );
        assert!(decode_grant_ticket(b"").is_none(), "empty payload");
    }
}
