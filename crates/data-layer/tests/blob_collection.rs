//! Blob collection over a node's one blob store: a payload stays while a
//! replica of any identity the node hosts references it and leaves at the
//! first run after none does, a forgotten replica freeing its payloads with
//! no removal of its own.

use std::time::Duration;

use anyhow::{Context, Result};
use data_layer::{identity_of, Contact, PodStore, SpawnOptions, SyncNode};
use iroh_blobs::Hash;
use pdn_types::{RecordId, RecordRef};
use test_utils::{
    eventually,
    pod::{
        create, create_at, device_of, held_records, host, invite, place_claim, reads, tickets,
        Person,
    },
};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

async fn node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: QUIET,
        blob_collection_interval: Duration::from_millis(200),
        ..SpawnOptions::memory()
    })
    .await
}

/// `member`'s claim at the id `seed` names, its payload `payload`.
async fn claim(
    node: &SyncNode,
    member: &Person,
    pod: pdn_types::PodId,
    seed: u8,
    payload: &[u8],
) -> Result<RecordRef> {
    let key = data_layer::RecordKey::Claim {
        member: member.id,
        id: RecordId::from_bytes([seed; 16]),
        mseq: data_layer::Seq::FIRST,
    };
    node.write_pod_entry(member.id, pod, PodStore::Records, &key.to_bytes(), payload)
        .await?;
    Ok(key.record())
}

/// Polls `member`'s write of `payload` at `key` until the store holds its
/// entry, then drops it before polling it again: whether that happened, or
/// the write finished first. A read is answered after every insert sent
/// before it, and the write is not polled while the read runs.
async fn dropped_mid_call(
    node: &SyncNode,
    member: &Person,
    pod: pdn_types::PodId,
    key: &data_layer::RecordKey,
    payload: &[u8],
) -> Result<bool> {
    let record = key.record();
    let bytes = key.to_bytes();
    let mut write =
        std::pin::pin!(node.write_pod_entry(member.id, pod, PodStore::Records, &bytes, payload));
    loop {
        let polled = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(write.as_mut(), cx))
        })
        .await;
        if let std::task::Poll::Ready(written) = polled {
            written?;
            return Ok(false);
        }
        let view = node.pod_record_view_of(member.id, pod, &record).await?;
        if view.placed(&record).is_some() {
            return Ok(true);
        }
        tokio::task::yield_now().await;
    }
}

/// The payloads a forgotten record store alone referenced leave the node at
/// the next run. Denied: one the record store of the identity's other pod
/// references too stays, and reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_no_replica_references_is_removed() -> Result<()> {
    let phone = node().await?;
    let (carol, _) = host(&phone).await?;
    let pod = create(&phone, &carol).await?;
    claim(&phone, &carol, pod, 1, b"lease scan").await?;
    claim(&phone, &carol, pod, 2, b"photo").await?;
    let own = create_at(&phone, &carol, [0x0c; 16]).await?;
    let kept = claim(&phone, &carol, own, 3, b"photo").await?;
    let (scan, picture) = (Hash::new(b"lease scan"), Hash::new(b"photo"));
    for hash in [scan, picture] {
        assert!(phone.holds_payload(hash).await?);
    }

    phone.forget_pod(carol.id, pod).await?;
    assert!(
        eventually(|| async { Ok(!phone.holds_payload(scan).await?) }).await?,
        "the payload no replica references stayed"
    );
    // Denied: the payload Carol's other pod references.
    assert!(phone.holds_payload(picture).await?);
    assert_eq!(
        phone
            .read_pod_record(carol.id, own, &kept)
            .await?
            .as_deref(),
        Some(&b"photo"[..])
    );

    phone.shutdown().await?;
    Ok(())
}

/// A payload a co-located identity's replica still references stays when
/// the other identity forgets its replica, and that identity reads it.
///
/// A payload only the forgetting identity's own pod referenced, forgotten
/// beside the shared one, orders the assertion after a run that removed
/// something.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_a_co_located_identity_references_stays() -> Result<()> {
    let tablet = node().await?;
    let (leisure, _) = host(&tablet).await?;
    let (work, _) = host(&tablet).await?;
    let pod = create(&tablet, &leisure).await?;
    invite(
        &tablet,
        &leisure,
        pod,
        &work,
        vec![device_of(&tablet, &work)?],
    )
    .await?;
    tablet
        .import_pod(work.id, pod, tickets(&tablet, &leisure, pod).await?)
        .await?;
    let shared = place_claim(&tablet, &leisure, pod, 1).await?;
    assert!(reads(&tablet, work.id, pod, shared).await?);
    let own = create_at(&tablet, &work, [0x0c; 16]).await?;
    claim(&tablet, &work, own, 2, b"only work's").await?;
    let (sentinel, claim) = (Hash::new(b"only work's"), Hash::new(b"claim"));

    tablet.forget_pod(work.id, pod).await?;
    tablet.forget_pod(work.id, own).await?;
    assert!(eventually(|| async { Ok(!tablet.holds_payload(sentinel).await?) }).await?);
    assert!(
        tablet.holds_payload(claim).await?,
        "a payload a co-located identity references was removed"
    );
    assert!(reads(&tablet, leisure.id, pod, shared).await?);

    tablet.shutdown().await?;
    Ok(())
}

/// After a restart, a node on a storage directory removes nothing until its
/// host has hosted its identities again and lets collection start, and
/// collects from then on.
///
/// Nothing is collected before the start, so no removal can order the
/// absence of one; the wait spans several runs at their interval.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_removes_nothing_before_its_host_lets_collection_start() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let on_dir = || {
        SyncNode::spawn(SpawnOptions {
            reconcile_interval: QUIET,
            pod_reconcile_interval: QUIET,
            blob_collection_interval: Duration::from_millis(200),
            ..SpawnOptions::on_directory(dir.path())
        })
    };
    let first = on_dir().await?;
    let carol = Person::generate();
    let pms = test_utils::host_identity(&first, carol.id).await?;
    first.record_hosting(carol.id, pms.namespace()).await?;
    let pod = create(&first, &carol).await?;
    claim(&first, &carol, pod, 1, b"photo").await?;
    first.shutdown().await?;
    drop(first);

    let second = on_dir().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        second.holds_payload(Hash::new(b"photo")).await?,
        "a payload was removed before the host let collection start"
    );
    second.provision_identity(carol.id).await?;
    let reopened = data_layer::PrivateMetadataStore::open(&second, carol.id, pms.namespace())
        .await?
        .expect("the PMS survives the restart");
    second.host_identity(carol.id, &reopened)?;
    second.start_blob_collection();
    // Collection runs from the start on: a payload nothing references goes.
    let stray = second.add_stray_payload_for_test(b"stray").await?;
    assert!(eventually(|| async { Ok(!second.holds_payload(stray).await?) }).await?);
    assert!(second.holds_payload(Hash::new(b"photo")).await?);
    second.shutdown().await?;
    Ok(())
}

/// A payload written while a collection run is under way stays through that
/// run, and reads.
///
/// The run is held once it has read the replicas, and the write lands in the
/// hold; a stray payload added beside it leaves in the same run, so the run
/// removed what nothing holds. The next run reaching its hold orders the
/// assertions after this run's removals.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_written_while_a_run_is_under_way_stays() -> Result<()> {
    let phone = node().await?;
    let (carol, _) = host(&phone).await?;
    let pod = create(&phone, &carol).await?;

    let run = phone.pause_next_collection_for_test();
    run.reached.notified().await;
    let photo = claim(&phone, &carol, pod, 1, b"photo").await?;
    let stray = phone.add_stray_payload_for_test(b"stray").await?;
    run.release.notify_one();
    let next = phone.pause_next_collection_for_test();
    next.reached.notified().await;
    next.release.notify_one();

    assert!(!phone.holds_payload(stray).await?);
    assert_eq!(
        phone
            .read_pod_record(carol.id, pod, &photo)
            .await?
            .as_deref(),
        Some(&b"photo"[..]),
        "a payload written during a run was removed"
    );

    phone.shutdown().await?;
    Ok(())
}

/// A payload whose write its caller dropped after the store took the entry
/// stays through a collection run under way, and reads.
///
/// The run is held once it has read the replicas, and the write is dropped
/// after its entry landed and before its call returned (`dropped_mid_call`).
/// A write that finishes within one poll is tried again under another id.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_whose_write_was_dropped_mid_call_stays() -> Result<()> {
    let phone = node().await?;
    let (carol, _) = host(&phone).await?;
    let pod = create(&phone, &carol).await?;

    let run = phone.pause_next_collection_for_test();
    run.reached.notified().await;
    let mut dropped = None;
    for seed in 1..=50u8 {
        let key = data_layer::RecordKey::Claim {
            member: carol.id,
            id: RecordId::from_bytes([seed; 16]),
            mseq: data_layer::Seq::FIRST,
        };
        let record = key.record();
        let payload = [b"photo ".as_slice(), &[seed]].concat();
        if dropped_mid_call(&phone, &carol, pod, &key, &payload).await? {
            dropped = Some((record, payload));
            break;
        }
    }
    let (record, payload) = dropped.context("every write finished within one poll")?;
    run.release.notify_one();
    let next = phone.pause_next_collection_for_test();
    next.reached.notified().await;
    next.release.notify_one();

    assert!(
        phone.holds_payload(Hash::new(&payload)).await?,
        "the payload of a dropped write was removed"
    );
    assert_eq!(
        phone.read_pod_record(carol.id, pod, &record).await?,
        Some(payload)
    );

    phone.shutdown().await?;
    Ok(())
}

/// A payload that arrives from a member device while a collection run is
/// under way stays through that run, and reads.
///
/// Held as the scenario above: the record arrives and its payload is fetched
/// in the hold, and a stray payload leaves in the same run. The record
/// arrives by a dial rather than the swarm, which may not have formed yet
/// with every pass out of reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_fetched_while_a_run_is_under_way_stays() -> Result<()> {
    let (alice_phone, bob_phone) = (node().await?, node().await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        pod,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    bob_phone
        .import_pod(bob.id, pod, tickets(&alice_phone, &alice, pod).await?)
        .await?;

    let run = bob_phone.pause_next_collection_for_test();
    run.reached.notified().await;
    let claim = place_claim(&alice_phone, &alice, pod, 1).await?;
    let alices = Contact::new(alice_phone.dial_handle().addr(), identity_of(alice.id));
    bob_phone
        .sync_pod_with_for_test(bob.id, pod, PodStore::Records, alices)
        .await?;
    if !reads(&bob_phone, bob.id, pod, claim).await? {
        anyhow::bail!(
            "the claim never read on bob's phone: records {:?}, its payload held {}, \
             in flight {}, holding {:?}",
            held_records(&bob_phone, bob.id, pod).await?,
            bob_phone.holds_payload(Hash::new(b"claim")).await?,
            bob_phone.pod_syncs_in_flight_report_for_test(pod).await?,
            bob_phone.pod_holding(bob.id, pod).await?,
        );
    }
    let stray = bob_phone.add_stray_payload_for_test(b"stray").await?;
    run.release.notify_one();
    let next = bob_phone.pause_next_collection_for_test();
    next.reached.notified().await;
    next.release.notify_one();

    assert!(!bob_phone.holds_payload(stray).await?);
    assert!(
        bob_phone
            .read_pod_record(bob.id, pod, &claim)
            .await?
            .is_some(),
        "a payload fetched during a run was removed"
    );

    for node in [alice_phone, bob_phone] {
        node.shutdown().await?;
    }
    Ok(())
}
