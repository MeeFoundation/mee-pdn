//! The record view over one device's replica of a cell's record store: what
//! the entries it holds read as, by the membership its replica of the
//! membership store folds into at each read. Every entry arrives by the
//! store-level writes the cells service performs, on the one device: no
//! session serves the record store, so the view's verdict on a relayed
//! entry is not asserted here.

use anyhow::Result;
use data_layer::{
    AnnouncementKeyPair, CellStore, EventKind, ForNothing, MemberDevice, MembershipKey, OpId,
    RecordKey, Seq, SyncNode, UnknownCell, Verdict,
};
use pdn_types::{CellId, PdnId, RecordId};
use test_utils::{host_identity, memory_node};

/// An identity whose `PdnId` derives from the announcement key pair beside it.
struct Identity {
    keys: AnnouncementKeyPair,
    id: PdnId,
}

async fn host(node: &SyncNode) -> Result<Identity> {
    let keys = AnnouncementKeyPair::generate();
    let id = keys.pdn_id();
    host_identity(node, id).await?;
    Ok(Identity { keys, id })
}

fn device_of(node: &SyncNode, identity: &Identity) -> Result<MemberDevice> {
    Ok(MemberDevice {
        node: node.node_id(),
        author: node.default_author(identity.id)?,
    })
}

async fn write(
    node: &SyncNode,
    writer: &Identity,
    cell: CellId,
    store: CellStore,
    key: Vec<u8>,
    payload: &[u8],
) -> Result<()> {
    node.write_cell_entry(writer.id, cell, store, &key, payload)
        .await
}

async fn statement(
    node: &SyncNode,
    writer: &Identity,
    cell: CellId,
    member: &Identity,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Devices {
        member: member.id,
        version: 1,
    };
    let payload = member.keys.device_statement(1, devices).encode();
    write(
        node,
        writer,
        cell,
        CellStore::Membership,
        key.to_bytes(),
        &payload,
    )
    .await
}

fn verdict_at(verdicts: &[(Vec<u8>, Verdict)], key: &RecordKey) -> Option<Verdict> {
    verdicts
        .iter()
        .find(|(held, _verdict)| *held == key.to_bytes())
        .map(|(_held, verdict)| *verdict)
}

/// A member's claim, and its operation on another member's mergeable-
/// document, read on its device, the claim once the device statement
/// listing the device arrives; a tombstone reads nothing. Paired denial:
/// the device's claim under the other member's name is held and read by
/// nothing.
#[allow(clippy::too_many_lines)] // one scenario: each read beside its denial
#[tokio::test(flavor = "multi_thread")]
async fn a_members_records_read_on_its_device_and_its_forgery_under_another_name_does_not(
) -> Result<()> {
    let phone = memory_node().await?;
    let alice = host(&phone).await?;
    let bob = host(&phone).await?;
    let founding = alice.keys.founding([0x5a; 16]);
    let cell = data_layer::cell_id_of(&alice.id, &founding.announcement_key, &founding.nonce);
    phone.create_cell(alice.id, cell).await?;
    let founded = MembershipKey::founded(alice.id).to_bytes();
    write(
        &phone,
        &alice,
        cell,
        CellStore::Membership,
        founded,
        &founding.encode(),
    )
    .await?;

    let blood_type = RecordKey::Claim {
        member: alice.id,
        id: RecordId::from_bytes([1; 16]),
        mseq: Seq::FIRST,
    };
    let records = CellStore::Records;
    write(&phone, &alice, cell, records, blood_type.to_bytes(), b"A+").await?;
    assert_eq!(
        phone
            .read_cell_record(alice.id, cell, &blood_type.record())
            .await?,
        None,
        "the claim read before any statement listed its device"
    );
    statement(
        &phone,
        &alice,
        cell,
        &alice,
        vec![device_of(&phone, &alice)?],
    )
    .await?;
    let joined = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::FIRST,
        kind: EventKind::Joined,
        actor: alice.id,
        actor_seq: Seq::FIRST,
    };
    let join_statement = bob.keys.join_statement(&cell, Seq::FIRST).encode();
    write(
        &phone,
        &alice,
        cell,
        CellStore::Membership,
        joined.to_bytes(),
        &join_statement,
    )
    .await?;
    statement(&phone, &alice, cell, &bob, vec![device_of(&phone, &bob)?]).await?;
    assert_eq!(
        phone
            .read_cell_record(alice.id, cell, &blood_type.record())
            .await?
            .as_deref(),
        Some(&b"A+"[..])
    );

    let milk = RecordKey::Operation {
        member: bob.id,
        id: RecordId::from_bytes([2; 16]),
        op: OpId {
            writer: alice.id,
            author: phone.default_author(alice.id)?,
            mseq: Seq::FIRST,
            op_seq: 1,
        },
    };
    write(&phone, &alice, cell, records, milk.to_bytes(), b"milk").await?;
    let operations = phone
        .read_cell_operations(alice.id, cell, &milk.record())
        .await?;
    assert_eq!(operations.len(), 1);
    assert_eq!(operations.first().map(|op| op.id.writer), Some(alice.id));
    assert_eq!(
        operations.first().map(|op| op.payload.as_slice()),
        Some(&b"milk"[..])
    );

    // Denied: a claim under Bob's name from Alice's device.
    let forged = RecordKey::Claim {
        member: bob.id,
        id: RecordId::from_bytes([3; 16]),
        mseq: Seq::FIRST,
    };
    write(&phone, &alice, cell, records, forged.to_bytes(), b"forged").await?;
    assert_eq!(
        phone
            .read_cell_record(alice.id, cell, &forged.record())
            .await?,
        None
    );
    let view = phone.cell_record_view(alice.id, cell).await?;
    let verdicts: Vec<(Vec<u8>, Verdict)> = view
        .verdicts()
        .map(|(entry, verdict)| (entry.key.clone(), verdict))
        .collect();
    assert_eq!(
        verdict_at(&verdicts, &forged),
        Some(Verdict::CountedForNothing(ForNothing::AuthorNotActorDevice)),
        "the forgery is not held as read by nothing"
    );
    let mut expected = vec![blood_type.record(), milk.record()];
    expected.sort();
    assert_eq!(view.records().copied().collect::<Vec<_>>(), expected);

    phone.forget_cell(alice.id, cell).await?;
    let tombstone = phone.cell_record_view(alice.id, cell).await;
    assert!(
        tombstone.is_err_and(|err| err.downcast_ref::<UnknownCell>().is_some()),
        "a tombstone's records read"
    );

    phone.shutdown().await?;
    Ok(())
}
