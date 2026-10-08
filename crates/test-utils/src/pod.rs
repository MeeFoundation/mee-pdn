//! A pod's entries as the pods service writes them, by the store-level
//! writes it performs, and the reads a scenario waits on.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, AnnouncementKeyPair, EventKind, MemberDevice, MemberState, MembershipKey,
    PodStore, PodTickets, PrivateMetadataStore, RecordKey, Seq, SyncNode,
};
use pdn_types::{PdnId, PodId, RecordId, RecordRef};

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

/// A fresh person hosted on `node`, with the PMS [`host_identity`]
/// gives it.
pub async fn host(node: &SyncNode) -> Result<(Person, PrivateMetadataStore)> {
    let person = Person::generate();
    let pms = host_identity(node, person.id).await?;
    Ok((person, pms))
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
    pod: PodId,
    key: MembershipKey,
    payload: Vec<u8>,
) -> Result<()> {
    node.write_pod_entry(
        writer.id,
        pod,
        PodStore::Membership,
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
    pod: PodId,
    member: &Person,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Devices {
        member: member.id,
        version: 1,
    };
    let payload = member.keys.device_statement(1, devices).encode();
    write(node, writer, pod, key, payload).await
}

/// `creator`'s pod on `node`: both stores, the created event and the
/// creator's first device statement.
pub async fn create(node: &SyncNode, creator: &Person) -> Result<PodId> {
    let creation = creator.keys.creation([0x5a; 16]);
    let pod = data_layer::pod_id_of(&creator.id, &creation.announcement_key, &creation.nonce);
    node.create_pod(creator.id, pod).await?;
    write(
        node,
        creator,
        pod,
        MembershipKey::created(creator.id),
        creation.encode(),
    )
    .await?;
    statement(node, creator, pod, creator, vec![device_of(node, creator)?]).await?;
    Ok(pod)
}

pub async fn tickets(node: &SyncNode, holder: &Person, pod: PodId) -> Result<PodTickets> {
    node.share_pod_tickets(holder.id, pod, AddrInfoOptions::Addresses)
        .await
}

/// The invite act for `newcomer` at its sequence 1, naming the inviter's
/// sequence 1, and the newcomer's first device statement, both written on
/// the inviter's node as the join dialogue writes them.
pub async fn invite(
    node: &SyncNode,
    inviter: &Person,
    pod: PodId,
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
    let join_statement = newcomer.keys.join_statement(&pod, Seq::FIRST);
    write(node, inviter, pod, key, join_statement.encode()).await?;
    statement(node, inviter, pod, newcomer, devices).await
}

/// `member`'s state as `holder`'s replica folds it; no member where the
/// replica is not held.
pub async fn state_on(node: &SyncNode, holder: PdnId, pod: PodId, member: PdnId) -> MemberState {
    match node.pod_membership(holder, pod).await {
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
    pod: PodId,
    member: PdnId,
    want: MemberState,
) -> Result<bool> {
    eventually(|| async { Ok(state_on(node, holder, pod, member).await == want) }).await
}

/// Whether `holder`'s replica comes to fold `device` among `member`'s.
pub async fn lists_device(
    node: &SyncNode,
    holder: PdnId,
    pod: PodId,
    member: PdnId,
    device: MemberDevice,
) -> Result<bool> {
    eventually(|| async {
        Ok(node
            .pod_membership(holder, pod)
            .await
            .is_ok_and(|membership| {
                membership
                    .member(&member)
                    .is_some_and(|folded| folded.devices.contains(&device))
            }))
    })
    .await
}

pub async fn folds_nobody(node: &SyncNode, holder: PdnId, pod: PodId) -> Result<bool> {
    Ok(node
        .pod_membership(holder, pod)
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
    pod: PodId,
    seed: u8,
) -> Result<RecordRef> {
    let key = RecordKey::Claim {
        member: member.id,
        id: RecordId::from_bytes([seed; 16]),
        mseq: Seq::FIRST,
    };
    node.write_pod_entry(member.id, pod, PodStore::Records, &key.to_bytes(), b"claim")
        .await?;
    Ok(key.record())
}

/// Whether `holder`'s record view comes to read `record`.
pub async fn reads(node: &SyncNode, holder: PdnId, pod: PodId, record: RecordRef) -> Result<bool> {
    eventually(|| async { Ok(node.read_pod_record(holder, pod, &record).await?.is_some()) }).await
}

/// Every entry `holder`'s record store holds now, as `key by author:
/// verdict`, so a denial that fails names what got through.
pub async fn held_records(node: &SyncNode, holder: PdnId, pod: PodId) -> Result<Vec<String>> {
    Ok(node
        .pod_record_view(holder, pod)
        .await?
        .verdicts()
        .map(|(entry, verdict)| {
            format!(
                "{} by {}: {verdict:?}",
                String::from_utf8_lossy(&entry.key),
                entry.author.fmt_short()
            )
        })
        .collect())
}
