//! A cell's entries as the cells service writes them, by the store-level
//! writes it performs, and the reads a scenario waits on.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, AnnouncementKeyPair, CellStore, CellTickets, EventKind, MemberDevice,
    MemberState, MembershipKey, PrivateMetadataStore, RecordKey, Seq, SyncNode,
};
use pdn_types::{CellId, PdnId, RecordId, RecordRef};

use crate::{eventually, host_identity};

/// An identity whose `PdnId` derives from the announcement key pair beside it.
pub struct Person {
    pub keys: AnnouncementKeyPair,
    pub id: PdnId,
}

impl Person {
    pub fn generate() -> Self {
        let keys = AnnouncementKeyPair::generate();
        let id = keys.pdn_id();
        Self { keys, id }
    }
}

/// A fresh person hosted on `node`, with the directory [`host_identity`]
/// gives it.
pub async fn host(node: &SyncNode) -> Result<(Person, PrivateMetadataStore)> {
    let person = Person::generate();
    let directory = host_identity(node, person.id).await?;
    Ok((person, directory))
}

/// `person`'s device on `node`: the node id and the author it writes with.
pub fn device_of(node: &SyncNode, person: &Person) -> Result<MemberDevice> {
    Ok(MemberDevice {
        node: node.node_id(),
        author: node.default_author(person.id)?,
    })
}

pub async fn write(
    node: &SyncNode,
    writer: &Person,
    cell: CellId,
    key: MembershipKey,
    payload: Vec<u8>,
) -> Result<()> {
    node.write_cell_entry(
        writer.id,
        cell,
        CellStore::Membership,
        &key.to_bytes(),
        &payload,
    )
    .await
}

/// `member`'s device statement at version 1, written from `writer`'s
/// device.
pub async fn statement(
    node: &SyncNode,
    writer: &Person,
    cell: CellId,
    member: &Person,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Devices {
        member: member.id,
        version: 1,
    };
    let payload = member.keys.device_statement(1, devices).encode();
    write(node, writer, cell, key, payload).await
}

/// `creator`'s cell on `node`: both stores, the founding event and the
/// creator's first device statement.
pub async fn found(node: &SyncNode, creator: &Person) -> Result<CellId> {
    let founding = creator.keys.founding([0x5a; 16]);
    let cell = data_layer::cell_id_of(&creator.id, &founding.announcement_key, &founding.nonce);
    node.create_cell(creator.id, cell).await?;
    write(
        node,
        creator,
        cell,
        MembershipKey::founded(creator.id),
        founding.encode(),
    )
    .await?;
    statement(
        node,
        creator,
        cell,
        creator,
        vec![device_of(node, creator)?],
    )
    .await?;
    Ok(cell)
}

pub async fn tickets(node: &SyncNode, holder: &Person, cell: CellId) -> Result<CellTickets> {
    node.share_cell_tickets(holder.id, cell, AddrInfoOptions::Addresses)
        .await
}

/// The invite act for `newcomer` at its sequence 1, naming the inviter's
/// sequence 1, and the newcomer's first device statement, both written on
/// the inviter's node as the join dialogue writes them.
pub async fn invite(
    node: &SyncNode,
    inviter: &Person,
    cell: CellId,
    newcomer: &Person,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Event {
        subject: newcomer.id,
        seq: Seq::FIRST,
        kind: EventKind::Joined,
        actor: inviter.id,
        actor_seq: Seq::FIRST,
    };
    let join_statement = newcomer.keys.join_statement(&cell, Seq::FIRST);
    write(node, inviter, cell, key, join_statement.encode()).await?;
    statement(node, inviter, cell, newcomer, devices).await
}

/// `member`'s state as `holder`'s replica folds it; no member where the
/// replica is not held.
pub async fn state_on(node: &SyncNode, holder: PdnId, cell: CellId, member: PdnId) -> MemberState {
    match node.cell_membership(holder, cell).await {
        Ok(membership) => membership
            .member(&member)
            .map(|member| member.state)
            .unwrap_or_default(),
        Err(_unknown) => MemberState::default(),
    }
}

/// Whether `holder`'s replica comes to fold `member` into `want`.
pub async fn lists(
    node: &SyncNode,
    holder: PdnId,
    cell: CellId,
    member: PdnId,
    want: MemberState,
) -> Result<bool> {
    eventually(|| async { Ok(state_on(node, holder, cell, member).await == want) }).await
}

pub async fn folds_nobody(node: &SyncNode, holder: PdnId, cell: CellId) -> Result<bool> {
    Ok(node
        .cell_membership(holder, cell)
        .await?
        .identities()
        .next()
        .is_none())
}

/// `member`'s claim at the id `seed` names, placed from its device on
/// `node` at its sequence 1.
pub async fn place_claim(
    node: &SyncNode,
    member: &Person,
    cell: CellId,
    seed: u8,
) -> Result<RecordRef> {
    let key = RecordKey::Claim {
        member: member.id,
        id: RecordId::from_bytes([seed; 16]),
        mseq: Seq::FIRST,
    };
    node.write_cell_entry(
        member.id,
        cell,
        CellStore::Records,
        &key.to_bytes(),
        b"claim",
    )
    .await?;
    Ok(key.record())
}

/// Whether `holder`'s record view comes to read `record`.
pub async fn reads(
    node: &SyncNode,
    holder: PdnId,
    cell: CellId,
    record: RecordRef,
) -> Result<bool> {
    eventually(|| async {
        Ok(node
            .read_cell_record(holder, cell, &record)
            .await?
            .is_some())
    })
    .await
}

pub async fn holds_no_record(node: &SyncNode, holder: PdnId, cell: CellId) -> Result<bool> {
    Ok(node
        .cell_record_view(holder, cell)
        .await?
        .verdicts()
        .next()
        .is_none())
}
