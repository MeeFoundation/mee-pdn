//! The record view over a device's replica of a pod's record store — what
//! the entries it holds read as, by the membership its replica of the
//! membership store folds into at each read — and the entries of either
//! store outside the key layout, held, read by nothing and listed with
//! their authors. Every entry arrives by the store-level writes the pods
//! service performs, and the tickets by hand.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    identity_of, AnnouncementKeyPair, Contact, EventKind, ForNothing, MemberDevice, MembershipKey,
    OpId, PodStore, RecordKey, Seq, SpawnOptions, SyncNode, UnknownEntry, UnknownPod, Verdict,
};
use pdn_types::{PdnId, PodId, RecordId};
use test_utils::{eventually, host_identity, memory_node, pod as c, TIMEOUT};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

const PLAIN: data_layer::MemberState = data_layer::MemberState {
    member: true,
    owner: false,
};

async fn quiet_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: QUIET,
        ..SpawnOptions::memory()
    })
    .await
}

/// Whether `holder`'s replicas come to list `entry` outside the key layout.
async fn lists_unknown(
    node: &SyncNode,
    holder: PdnId,
    pod: PodId,
    entry: &UnknownEntry,
) -> Result<bool> {
    eventually(|| async { Ok(node.list_pod_unknown(holder, pod).await?.contains(entry)) }).await
}

/// Whether `pod`'s stores come to have nothing in flight on every one of
/// `nodes` — no exchange running, held or due to redial — in two reads in
/// a row: a dial one node still makes lands on another between two reads.
async fn settle(nodes: &[&SyncNode], pod: PodId) -> Result<bool> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut quiet_reads = 0_u8;
    while std::time::Instant::now() < deadline {
        let mut in_flight = 0_usize;
        for node in nodes {
            in_flight = in_flight.saturating_add(node.pod_syncs_in_flight_for_test(pod).await?);
        }
        quiet_reads = if in_flight == 0 {
            quiet_reads.saturating_add(1)
        } else {
            0
        };
        if quiet_reads == 2 {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// The entries received and sent by the session `holder`'s replica of
/// `pod`'s `store` on `from` dials next to `callee`'s on `to`.
async fn next_session_exchanges(
    (from, holder): (&SyncNode, &c::Person),
    pod: PodId,
    store: PodStore,
    (to, callee): (&SyncNode, &c::Person),
) -> Result<Option<(usize, usize)>> {
    let mut sessions = from.watch_pod_sessions(holder.id, pod, store).await?;
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee.id));
    from.sync_pod_with_for_test(holder.id, pod, store, contact)
        .await?;
    let session = sessions.next_with(to.node_id(), true, TIMEOUT).await?;
    Ok(session.and_then(|session| session.exchanged.ok()))
}

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
    pod: PodId,
    store: PodStore,
    key: Vec<u8>,
    payload: &[u8],
) -> Result<()> {
    node.write_pod_entry(writer.id, pod, store, &key, payload)
        .await
}

async fn statement(
    node: &SyncNode,
    writer: &Identity,
    pod: PodId,
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
        pod,
        PodStore::Membership,
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
    let pod = data_layer::pod_id_of(&alice.id, &founding.announcement_key, &founding.nonce);
    phone.create_pod(alice.id, pod).await?;
    let founded = MembershipKey::founded(alice.id).to_bytes();
    write(
        &phone,
        &alice,
        pod,
        PodStore::Membership,
        founded,
        &founding.encode(),
    )
    .await?;

    let blood_type = RecordKey::Claim {
        member: alice.id,
        id: RecordId::from_bytes([1; 16]),
        mseq: Seq::FIRST,
    };
    let records = PodStore::Records;
    write(&phone, &alice, pod, records, blood_type.to_bytes(), b"A+").await?;
    assert_eq!(
        phone
            .read_pod_record(alice.id, pod, &blood_type.record())
            .await?,
        None,
        "the claim read before any statement listed its device"
    );
    statement(
        &phone,
        &alice,
        pod,
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
    let join_statement = bob.keys.join_statement(&pod, Seq::FIRST).encode();
    write(
        &phone,
        &alice,
        pod,
        PodStore::Membership,
        joined.to_bytes(),
        &join_statement,
    )
    .await?;
    statement(&phone, &alice, pod, &bob, vec![device_of(&phone, &bob)?]).await?;
    assert_eq!(
        phone
            .read_pod_record(alice.id, pod, &blood_type.record())
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
    write(&phone, &alice, pod, records, milk.to_bytes(), b"milk").await?;
    let operations = phone
        .read_pod_operations(alice.id, pod, &milk.record())
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
    write(&phone, &alice, pod, records, forged.to_bytes(), b"forged").await?;
    assert_eq!(
        phone
            .read_pod_record(alice.id, pod, &forged.record())
            .await?,
        None
    );
    let view = phone.pod_record_view(alice.id, pod).await?;
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

    phone.forget_pod(alice.id, pod).await?;
    let tombstone = phone.pod_record_view(alice.id, pod).await;
    assert!(
        tombstone.is_err_and(|err| err.downcast_ref::<UnknownPod>().is_some()),
        "a tombstone's records read"
    );

    phone.shutdown().await?;
    Ok(())
}

/// A claim a member relays reads on another member's device as its
/// author's, while its author's device is offline. Denied: the relaying
/// member's own entry at the claim's key, newer though it is, is held and
/// read by nothing, and a holder of the relay's tickets that is no member
/// takes nothing.
#[allow(clippy::too_many_lines)] // one scenario: the relay, the forgery and the denial
#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_claim_reads_as_its_authors_and_the_relays_entry_at_its_key_by_nothing(
) -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone) = (
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
    );
    let (alice, _) = c::host(&alice_phone).await?;
    let (bob, _) = c::host(&bob_phone).await?;
    let (carol, _) = c::host(&carol_phone).await?;
    let (dave, _) = c::host(&dave_phone).await?;
    let pod = c::found(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        c::invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![c::device_of(phone, member)?],
        )
        .await?;
    }
    bob_phone
        .import_pod(bob.id, pod, c::tickets(&alice_phone, &alice, pod).await?)
        .await?;
    let claim = c::place_claim(&alice_phone, &alice, pod, 1).await?;
    assert!(c::reads(&bob_phone, bob.id, pod, claim).await?);
    let at_its_key = RecordKey::Claim {
        member: alice.id,
        id: claim.id,
        mseq: Seq::FIRST,
    };
    bob_phone
        .write_pod_entry(
            bob.id,
            pod,
            PodStore::Records,
            &at_its_key.to_bytes(),
            b"forged",
        )
        .await?;
    // Everything Carol's phone needs, payloads included, is on Bob's phone
    // before Alice's goes.
    assert!(c::lists(&bob_phone, bob.id, pod, carol.id, PLAIN).await?);
    alice_phone.shutdown().await?;

    let from_bob = c::tickets(&bob_phone, &bob, pod).await?;
    carol_phone
        .import_pod(carol.id, pod, from_bob.clone())
        .await?;
    dave_phone.import_pod(dave.id, pod, from_bob).await?;
    assert!(c::reads(&carol_phone, carol.id, pod, claim).await?);
    assert_eq!(
        carol_phone
            .read_pod_record(carol.id, pod, &claim)
            .await?
            .as_deref(),
        Some(&b"claim"[..]),
        "the relay's entry at the claim's key read"
    );
    let bobs_author = bob_phone.default_author(bob.id)?;
    let held = carol_phone.pod_record_view(carol.id, pod).await?;
    let forged = held
        .verdicts()
        .find(|(entry, _verdict)| entry.author == bobs_author)
        .map(|(_entry, verdict)| verdict);
    assert_eq!(
        forged,
        Some(Verdict::CountedForNothing(ForNothing::AuthorNotActorDevice)),
        "the relay's entry is not held as read by nothing"
    );
    // Denied: the holder of the relay's tickets that is no member.
    assert!(c::holds_no_record(&dave_phone, dave.id, pod).await?);

    for node in [bob_phone, carol_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// An entry a member writes outside the key layout of either store reaches
/// every member device and is listed there with its author, every record
/// and every member reading as before, and a later session between two
/// member devices finds no difference. Denied: a holder of both tickets
/// that is no member lists nothing.
#[allow(clippy::too_many_lines)] // one scenario: both stores' unknown entries beside the denial
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_entry_from_a_member_converges_and_changes_nothing() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone) = (
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
    );
    let (alice, _) = c::host(&alice_phone).await?;
    let (bob, _) = c::host(&bob_phone).await?;
    let (carol, _) = c::host(&carol_phone).await?;
    let (dave, _) = c::host(&dave_phone).await?;
    let pod = c::found(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&carol_phone, &carol)] {
        c::invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![c::device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = c::tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in [
        (&bob_phone, &bob),
        (&carol_phone, &carol),
        (&dave_phone, &dave),
    ] {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    let claim = c::place_claim(&alice_phone, &alice, pod, 1).await?;
    assert!(c::reads(&carol_phone, carol.id, pod, claim).await?);
    // Bob's phone serves the pulls of its writes once it knows the readers.
    for (phone, reader) in [(&alice_phone, &alice), (&carol_phone, &carol)] {
        let device = c::device_of(phone, reader)?;
        assert!(c::lists_device(&bob_phone, bob.id, pod, reader.id, device).await?);
    }

    let bobs_author = bob_phone.default_author(bob.id)?;
    let outside = UnknownEntry {
        store: PodStore::Records,
        key: b"ext/anything".to_vec(),
        author: bobs_author,
    };
    let record_key_in_membership = UnknownEntry {
        store: PodStore::Membership,
        key: RecordKey::Claim {
            member: bob.id,
            id: RecordId::from_bytes([7; 16]),
            mseq: Seq::FIRST,
        }
        .to_bytes(),
        author: bobs_author,
    };
    for entry in [&outside, &record_key_in_membership] {
        bob_phone
            .write_pod_entry(bob.id, pod, entry.store, &entry.key, b"unknown")
            .await?;
    }
    for (phone, holder) in [(&alice_phone, &alice), (&carol_phone, &carol)] {
        for entry in [&outside, &record_key_in_membership] {
            assert!(
                lists_unknown(phone, holder.id, pod, entry).await?,
                "an entry outside the key layout did not reach a member's device"
            );
        }
        assert!(c::reads(phone, holder.id, pod, claim).await?);
        assert_eq!(c::state_on(phone, holder.id, pod, bob.id).await, PLAIN);
    }
    assert!(settle(&[&alice_phone, &bob_phone, &carol_phone], pod).await?);
    for store in [PodStore::Membership, PodStore::Records] {
        for other in [(&bob_phone, &bob), (&carol_phone, &carol)] {
            let found = next_session_exchanges((&alice_phone, &alice), pod, store, other).await?;
            assert_eq!(
                found,
                Some((0, 0)),
                "{store:?}: a later session found a difference"
            );
        }
    }
    // Denied: the ticket holder that is no member.
    assert!(dave_phone.list_pod_unknown(dave.id, pod).await?.is_empty());

    for node in [alice_phone, bob_phone, carol_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// An entry outside the key layout that a member relays, authored by a key
/// no member's statement lists, is held on every member device and listed
/// with that author, changes no record and no member, and a later session
/// between two member devices finds no difference.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_entry_is_held_whoever_authored_it() -> Result<()> {
    let (alice_phone, bob_phone, dave_phone) = (
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
    );
    let (alice, _) = c::host(&alice_phone).await?;
    let (bob, _) = c::host(&bob_phone).await?;
    let (dave, _) = c::host(&dave_phone).await?;
    let pod = c::found(&alice_phone, &alice).await?;
    for (phone, member) in [(&bob_phone, &bob), (&dave_phone, &dave)] {
        c::invite(
            &alice_phone,
            &alice,
            pod,
            member,
            vec![c::device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = c::tickets(&alice_phone, &alice, pod).await?;
    for (phone, holder) in [(&bob_phone, &bob), (&dave_phone, &dave)] {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    let claim = c::place_claim(&alice_phone, &alice, pod, 1).await?;
    assert!(c::reads(&dave_phone, dave.id, pod, claim).await?);
    // Dave's phone serves the pulls of its writes once it knows the readers.
    for (phone, reader) in [(&alice_phone, &alice), (&bob_phone, &bob)] {
        let device = c::device_of(phone, reader)?;
        assert!(c::lists_device(&dave_phone, dave.id, pod, reader.id, device).await?);
    }

    let stranger = dave_phone.create_author(dave.id).await?;
    let relayed = UnknownEntry {
        store: PodStore::Records,
        key: b"ext/anything".to_vec(),
        author: stranger,
    };
    dave_phone
        .write_pod_entry_as_for_test(dave.id, pod, relayed.store, stranger, &relayed.key, b"x")
        .await?;
    for (phone, holder) in [(&alice_phone, &alice), (&bob_phone, &bob)] {
        assert!(
            lists_unknown(phone, holder.id, pod, &relayed).await?,
            "a member device did not hold an unknown entry of an unlisted author"
        );
        assert!(c::reads(phone, holder.id, pod, claim).await?);
        assert_eq!(c::state_on(phone, holder.id, pod, dave.id).await, PLAIN);
    }
    assert!(settle(&[&alice_phone, &bob_phone, &dave_phone], pod).await?);
    for store in [PodStore::Membership, PodStore::Records] {
        for other in [(&bob_phone, &bob), (&dave_phone, &dave)] {
            let found = next_session_exchanges((&alice_phone, &alice), pod, store, other).await?;
            assert_eq!(
                found,
                Some((0, 0)),
                "{store:?}: a later session found a difference"
            );
        }
    }

    for node in [alice_phone, bob_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}
