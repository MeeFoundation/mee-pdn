//! The runtime's observation surface (the observation spec of `pdn-node`):
//! numbered records of a pod's two stores, taken as the stores produce them
//! whatever the consumer's pace.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use pdn_node::{
    observation::{ObservedStore, PodRecord, PodSubscription, RecordBody},
    IdentityService as _, JoinRefused, PdnId, PodAct, PodId, PodsService as _, RecordKind, Runtime,
    SpawnOptions, UnknownIdentity, UnknownPod,
};
use test_utils::{eventually, TIMEOUT};

mod common;
use common::memory_runtime;

/// A pod with `owner` its creator on `owner_runtime` and `member` joined
/// from `member_runtime`.
async fn pod_of_two(
    owner_runtime: &Runtime,
    owner: PdnId,
    member_runtime: &Runtime,
    member: PdnId,
) -> Result<PodId> {
    let pod = owner_runtime.pods().create(owner).await?;
    let invite = owner_runtime.pods().invite(owner, pod, None).await?;
    member_runtime.pods().join(member, invite).await?;
    Ok(pod)
}

fn is<E: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> bool {
    err.downcast_ref::<E>().is_some()
}

/// Places an immutable-document of `payload`, which no other entry
/// carries, and answers its key from the writer's listing.
async fn place(runtime: &Runtime, identity: PdnId, pod: PodId, payload: &[u8]) -> Result<String> {
    runtime
        .pods()
        .put_record(identity, pod, RecordKind::ImmutableDocument, payload)
        .await?;
    runtime
        .pod_record_listing_for_test(identity, pod, true)
        .await?
        .into_iter()
        .find(|entry| entry.payload.as_deref() == Some(payload))
        .map(|entry| entry.key)
        .context("the writer does not list its own record")
}

fn insert_key(record: &PodRecord) -> Option<&str> {
    match &record.body {
        RecordBody::InsertLocal { key, .. } | RecordBody::InsertRemote { key, .. } => Some(key),
        _ => None,
    }
}

/// Reads up to and including the first record `until` accepts.
async fn read_until(
    subscription: &mut PodSubscription,
    mut until: impl FnMut(&PodRecord) -> bool,
) -> Result<Vec<PodRecord>> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut read = Vec::new();
    loop {
        let record = tokio::time::timeout_at(deadline, subscription.next())
            .await
            .context("the record did not arrive within the wait")?
            .context("the subscription ended")?;
        let done = until(&record);
        read.push(record);
        if done {
            return Ok(read);
        }
    }
}

/// Reads until no record arrives for `quiet`.
async fn read_until_quiet(subscription: &mut PodSubscription, quiet: Duration) -> Vec<PodRecord> {
    let mut read = Vec::new();
    while let Ok(Some(record)) = tokio::time::timeout(quiet, subscription.next()).await {
        read.push(record);
    }
    read
}

fn is_closed_records(record: &PodRecord) -> bool {
    record.store == ObservedStore::Records && record.body == RecordBody::Closed
}

/// `member` joins `pod` again from `owner`'s invite; its subscription
/// opened before the departure then reads up to the joined event, which
/// reaches the membership store.
async fn join_again(
    owner_runtime: &Runtime,
    owner: PdnId,
    member_runtime: &Runtime,
    member: PdnId,
    pod: PodId,
) -> Result<()> {
    let invite = owner_runtime.pods().invite(owner, pod, None).await?;
    member_runtime.pods().join(member, invite).await?;
    Ok(())
}

fn is_joined_event(record: &PodRecord) -> bool {
    record.store == ObservedStore::Membership
        && matches!(&record.body, RecordBody::InsertRemote { key, .. } if key.contains("/joined/"))
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

/// Reads until `count` remote inserts on the record store have been read.
async fn read_inserts(subscription: &mut PodSubscription, count: usize) -> Result<Vec<PodRecord>> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut read = Vec::new();
    let mut inserts = 0;
    while inserts < count {
        let record = tokio::time::timeout_at(deadline, subscription.next())
            .await
            .context("the records did not arrive within the wait")?
            .context("the subscription ended")?;
        if record.store == ObservedStore::Records
            && matches!(record.body, RecordBody::InsertRemote { .. })
        {
            inserts += 1;
        }
        read.push(record);
    }
    Ok(read)
}

/// A consumer that reads nothing for 5 seconds while sessions deliver
/// records, then drains, receives every record with no `lagged` and no gap,
/// each stamped before the read: an insert after the first write, a
/// finished session after its finish. The wait for arrival reads the record
/// listing, which starts no session; an entry can also arrive over the swarm,
/// outside any session, so an insert is not paired with one. Fails with `t`
/// stamped at the read.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_that_reads_late_receives_the_time_of_production() -> Result<()> {
    const RECORDS: usize = 50;
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let mut subscription = alice_phone.observe_pod_for_test(alice, pod).await?;
    let idle_from = Instant::now();
    let written_from = now_micros();

    for index in 0..RECORDS {
        bob_phone
            .pods()
            .put_record(
                bob,
                pod,
                RecordKind::ImmutableDocument,
                &index.to_le_bytes(),
            )
            .await?;
    }
    assert!(
        eventually(|| async {
            Ok(alice_phone
                .pod_record_listing_for_test(alice, pod, true)
                .await
                .is_ok_and(|listing| listing.len() == RECORDS))
        })
        .await?
    );
    tokio::time::sleep(Duration::from_secs(5).saturating_sub(idle_from.elapsed())).await;
    let read_from = now_micros();
    let read = read_inserts(&mut subscription, RECORDS).await?;

    let seqs: Vec<u64> = read.iter().map(|record| record.seq).collect();
    let expected: Vec<u64> = (0..).take(read.len()).collect();
    assert_eq!(seqs, expected, "no gap from 0");
    assert!(read.iter().all(|record| record.body != RecordBody::Lagged));
    for record in &read {
        let produced_from = match record.body {
            RecordBody::InsertRemote { .. } => written_from,
            RecordBody::SyncFinished { finished, .. } => finished,
            _ => continue,
        };
        assert!(
            produced_from <= record.t && record.t < read_from,
            "record {} stamped at {}, outside [{produced_from}, {read_from})",
            record.seq,
            record.t
        );
    }

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A subscription yields a written record's arrival while another call
/// holds the runtime's state lock. Fails with the subscription reading under
/// the lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_state_lock_does_not_stall_a_subscription() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let mut subscription = alice_phone.observe_pod_for_test(alice, pod).await?;

    let (acquired, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let hold = alice_phone.hold_state_lock_for_test(Arc::clone(&acquired), Arc::clone(&release));
    let observed = async {
        acquired.notified().await;
        let read = async {
            bob_phone
                .pods()
                .put_record(bob, pod, RecordKind::ImmutableDocument, b"under the lock")
                .await?;
            read_inserts(&mut subscription, 1).await
        }
        .await;
        release.notify_one();
        read
    };
    let ((), read) = tokio::join!(hold, observed);
    read?;

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member's subscription reports the remote insert of a record another
/// member places. Paired denial: a co-located identity holding no store of
/// the pod is refused, as is an identity this node does not host. Fails
/// with the subscription opened on whichever hosted identity holds the pod.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_is_served_and_a_co_located_identity_without_the_pod_is_refused() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    let mut subscription = bob_phone.observe_pod_for_test(bob, pod).await?;
    // Denied (co-located, no store of the pod).
    let refused = bob_phone.observe_pod_for_test(carol, pod).await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));
    // Denied (not hosted here).
    let refused = bob_phone.observe_pod_for_test(alice, pod).await;
    assert!(refused.is_err_and(|err| is::<UnknownIdentity>(&err)));

    let key = place(&alice_phone, alice, pod, b"served").await?;
    let read = read_inserts(&mut subscription, 1).await?;
    assert_eq!(read.last().and_then(insert_key), Some(key.as_str()));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A second subscription to the same pod carries a new identifier and
/// counts from 0. Fails with the identifier fixed.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_subscription_has_a_new_identifier() -> Result<()> {
    let alice_phone = memory_runtime().await?;
    let alice = alice_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;

    let mut first = alice_phone.observe_pod_for_test(alice, pod).await?;
    let mut second = alice_phone.observe_pod_for_test(alice, pod).await?;
    assert_ne!(first.id(), second.id());
    place(&alice_phone, alice, pod, b"counted twice").await?;
    for subscription in [&mut first, &mut second] {
        let read = read_until(subscription, |_| true).await?;
        let record = read.first().context("a record")?;
        assert_eq!((record.sub.as_str(), record.seq), (subscription.id(), 0));
    }

    alice_phone.shutdown().await?;
    Ok(())
}

/// The writer's local insert and the reader's remote insert of one record
/// carry the same key, content hash and entry timestamp. Fails with the
/// local insert stamped with the runtime's clock rather than the entry's.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_and_a_remote_insert_carry_the_same_key_hash_and_timestamp() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let mut written = alice_phone.observe_pod_for_test(alice, pod).await?;
    let mut received = bob_phone.observe_pod_for_test(bob, pod).await?;

    place(&alice_phone, alice, pod, b"one record").await?;
    let local = read_until(&mut written, |record| {
        record.store == ObservedStore::Records
            && matches!(record.body, RecordBody::InsertLocal { .. })
    })
    .await?;
    let remote = read_inserts(&mut received, 1).await?;
    let (
        Some(RecordBody::InsertLocal {
            key,
            entry_ts,
            hash,
            ..
        }),
        Some(RecordBody::InsertRemote {
            key: remote_key,
            entry_ts: remote_ts,
            hash: remote_hash,
            ..
        }),
    ) = (
        local.last().map(|record| &record.body),
        remote.last().map(|record| &record.body),
    )
    else {
        anyhow::bail!("no insert read");
    };
    assert_eq!((key, hash, entry_ts), (remote_key, remote_hash, remote_ts));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A joining member's sessions with the creator yield, on the creator's
/// subscription, a `sync_finished` naming the membership store and one
/// naming the record store, each with the joiner's node as the
/// counterpart. Fails with the membership store's events labelled as the
/// record store's.
#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_two_stores_are_told_apart() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let mut subscription = alice_phone.observe_pod_for_test(alice, pod).await?;

    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, invite).await?;
    let joiner = bob_phone.node_id().to_string();
    let mut unseen = HashSet::from([ObservedStore::Membership, ObservedStore::Records]);
    read_until(&mut subscription, |record| {
        if matches!(&record.body, RecordBody::SyncFinished { peer, ok: true, .. } if *peer == joiner)
        {
            unseen.remove(&record.store);
        }
        unseen.is_empty()
    })
    .await?;

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Departure {
    Removed,
    Left,
}

/// `bob`'s subscription closes its record stream at the departure, takes
/// no record placed afterwards, and goes on reporting the membership store.
/// The sentinel is `bob` joining again: the join's catch-up carries the
/// later record into its new record store, which a new subscription would
/// observe, and its joined event reaches the old one.
async fn a_departure_closes_the_record_stream(departure: Departure) -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let mut subscription = bob_phone.observe_pod_for_test(bob, pod).await?;

    let departed = match departure {
        Departure::Removed => {
            alice_phone
                .pods()
                .act(alice, pod, PodAct::Remove(bob))
                .await
        }
        Departure::Left => bob_phone.pods().act(bob, pod, PodAct::Leave).await,
    };
    departed?;
    read_until(&mut subscription, is_closed_records).await?;
    let later = place(&alice_phone, alice, pod, b"after the departure").await?;
    join_again(&alice_phone, alice, &bob_phone, bob, pod).await?;
    let listing = bob_phone
        .pod_record_listing_for_test(bob, pod, true)
        .await?;
    assert!(
        listing.iter().any(|entry| entry.key == later),
        "the later record did not reach the member's node"
    );
    let mut after = read_until(&mut subscription, is_joined_event).await?;
    after.extend(read_until_quiet(&mut subscription, Duration::from_secs(1)).await);
    assert!(
        after
            .iter()
            .all(|record| record.store == ObservedStore::Membership),
        "a record store's record after its `closed`: {after:?}"
    );

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A removal the removed member's node folds closes its record stream; no
/// record placed afterwards reaches the subscription, while its membership
/// store's records continue. Fails with a store's end not reported.
#[tokio::test(flavor = "multi_thread")]
async fn a_remove_closes_the_removed_members_record_stream() -> Result<()> {
    a_departure_closes_the_record_stream(Departure::Removed).await
}

/// A leave closes the leaving member's record stream, and the membership
/// store's records continue. Fails with a store's end not reported.
#[tokio::test(flavor = "multi_thread")]
async fn a_leave_closes_the_record_stream() -> Result<()> {
    a_departure_closes_the_record_stream(Departure::Left).await
}

/// A former member's subscription to the pod whose record store it has
/// forgotten starts with `closed` for the record store, and the membership
/// store's records follow. Fails with a tombstone's missing record store
/// not reported.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_on_a_tombstone_starts_closed() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    bob_phone.pods().act(bob, pod, PodAct::Leave).await?;

    let mut subscription = bob_phone.observe_pod_for_test(bob, pod).await?;
    let first = read_until(&mut subscription, |_| true).await?;
    assert!(first.first().is_some_and(is_closed_records), "{first:?}");
    join_again(&alice_phone, alice, &bob_phone, bob, pod).await?;
    read_until(&mut subscription, is_joined_event).await?;

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A runtime's stop ends its subscriptions with no `closed` record, and a
/// subscription on the runtime started again on the same directory carries
/// a new identifier and counts from 0. Fails with the stop left to end each
/// store's stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_runtime_ends_its_subscriptions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_phone = Runtime::spawn(SpawnOptions::on_directory(dir.path())).await?;
    let alice = alice_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let mut subscription = alice_phone.observe_pod_for_test(alice, pod).await?;
    place(&alice_phone, alice, pod, b"before the stop").await?;
    read_until(&mut subscription, |record| {
        matches!(record.body, RecordBody::InsertLocal { .. })
    })
    .await?;

    alice_phone.shutdown().await?;
    let mut rest = Vec::new();
    while let Some(record) = tokio::time::timeout(TIMEOUT, subscription.next())
        .await
        .context("the subscription did not end with the runtime")?
    {
        rest.push(record);
    }
    assert!(
        rest.iter().all(|record| record.body != RecordBody::Closed),
        "{rest:?}"
    );

    let alice_phone = Runtime::spawn(SpawnOptions::on_directory(dir.path())).await?;
    assert!(
        eventually(|| async { Ok(alice_phone.observe_pod_for_test(alice, pod).await.is_ok()) })
            .await?,
        "the restarted runtime does not host the pod"
    );
    let mut again = alice_phone.observe_pod_for_test(alice, pod).await?;
    assert_ne!(again.id(), subscription.id());
    place(&alice_phone, alice, pod, b"after the restart").await?;
    let first = read_until(&mut again, |_| true).await?;
    assert_eq!(first.first().map(|record| record.seq), Some(0));

    alice_phone.shutdown().await?;
    Ok(())
}

/// A reader's record listing holds a placed record `counted`, with its
/// payload, beside a forged entry at its key under an author no statement
/// lists, `for_nothing`. Paired denials: a co-located identity holding no
/// store of the pod, and the reader once removed, are refused the listing.
/// Fails with the listing merged by key, or with a refusal answered as an
/// empty listing.
#[tokio::test(flavor = "multi_thread")]
async fn the_listing_holds_a_placed_record_beside_a_forged_entry_at_its_key() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    let key = place(&alice_phone, alice, pod, b"placed").await?;
    alice_phone
        .forge_pod_record_for_test(alice, pod, key.as_bytes(), b"forged")
        .await?;
    assert!(
        eventually(|| async {
            let listing = bob_phone
                .pod_record_listing_for_test(bob, pod, true)
                .await?;
            Ok(listing
                .iter()
                .filter(|entry| entry.key == key && entry.present)
                .count()
                == 2)
        })
        .await?,
        "both entries did not reach the reader"
    );
    let listing = bob_phone
        .pod_record_listing_for_test(bob, pod, true)
        .await?;
    let at_key = |verdict: &str| {
        listing
            .iter()
            .find(|entry| entry.key == key && entry.verdict == verdict)
            .map(|entry| (entry.payload.as_deref(), entry.reason.as_deref()))
    };
    assert_eq!(at_key("counted"), Some((Some(&b"placed"[..]), None)));
    assert_eq!(
        at_key("for_nothing"),
        Some((Some(&b"forged"[..]), Some("author_not_actor_device")))
    );
    // Denied (co-located, no store of the pod).
    let refused = bob_phone
        .pod_record_listing_for_test(carol, pod, true)
        .await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Remove(bob))
        .await?;
    // Denied (removed).
    assert!(
        eventually(|| async {
            let refused = bob_phone.pod_record_listing_for_test(bob, pod, true).await;
            Ok(refused.is_err_and(|err| is::<UnknownPod>(&err)))
        })
        .await?,
        "the removed member is not refused the listing"
    );

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member's record listing starts no session within 10 seconds on a
/// 1-hour pod interval, while a promotion in the same arrangement starts
/// one that is reported (the positive control). Fails with the listing
/// replaced by a call that dials.
#[tokio::test(flavor = "multi_thread")]
async fn the_listing_starts_no_session() -> Result<()> {
    const WINDOW: Duration = Duration::from_secs(10);
    let hourly = || {
        Runtime::spawn(SpawnOptions {
            pod_reconcile_interval: Duration::from_secs(3_600),
            ..SpawnOptions::memory()
        })
    };
    let (alice_phone, bob_phone) = (hourly().await?, hourly().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    place(&alice_phone, alice, pod, b"listed").await?;
    let mut subscription = alice_phone.observe_pod_for_test(alice, pod).await?;
    // The join's own sessions end before the window opens.
    read_until_quiet(&mut subscription, Duration::from_secs(3)).await;

    let listing = alice_phone
        .pod_record_listing_for_test(alice, pod, true)
        .await?;
    assert_eq!(listing.len(), 1);
    let window = read_until_quiet(&mut subscription, WINDOW).await;
    assert!(
        window
            .iter()
            .all(|record| !matches!(record.body, RecordBody::SyncFinished { .. })),
        "a session within the window: {window:?}"
    );

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let session = tokio::time::timeout(
        WINDOW,
        read_until(&mut subscription, |record| {
            matches!(record.body, RecordBody::SyncFinished { .. })
        }),
    )
    .await;
    assert!(
        session.is_ok_and(|read| read.is_ok()),
        "the asked-for session was not reported"
    );

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

const HELD: usize = 1_000;

/// The control's pod: on a CI runner a catch-up of [`HELD`] records takes
/// about the whole of [`pdn_node::JOIN_CATCH_UP_TIMEOUT`].
const CONTROL_HELD: usize = 100;

/// A pod `alice` created on `alice_phone` holding `held` records of
/// distinct payloads, their keys, and an invite to it.
async fn pod_holding_records(
    alice_phone: &Runtime,
    alice: PdnId,
    held: usize,
) -> Result<(PodId, HashSet<String>, pdn_node::PodInvite)> {
    let pod = alice_phone.pods().create(alice).await?;
    for index in 0..held {
        alice_phone
            .pods()
            .put_record(
                alice,
                pod,
                RecordKind::ImmutableDocument,
                &index.to_le_bytes(),
            )
            .await?;
    }
    let keys: HashSet<String> = alice_phone
        .pod_record_listing_for_test(alice, pod, true)
        .await?
        .into_iter()
        .map(|entry| entry.key)
        .collect();
    assert_eq!(keys.len(), held);
    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    Ok((pod, keys, invite))
}

/// A joiner through the observed join of a pod holding 1,000 records has
/// an `insert_remote` of each before the record-store `sync_finished` of
/// the session that carried it, from `seq` 0 with no gap. Fails with the
/// observed join replaced by `join` followed by a subscription (the
/// control below).
#[tokio::test(flavor = "multi_thread")]
async fn a_joiners_first_session_is_reported_from_its_first_insert() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let (pod, keys, invite) = pod_holding_records(&alice_phone, alice, HELD).await?;

    let (joined, mut subscription) = bob_phone.join_pod_observed_for_test(bob, invite).await?;
    assert_eq!(joined, pod);
    // Per sending node, the inserts its next record-store session has yet
    // to finish.
    let mut carrying: HashMap<String, Vec<String>> = HashMap::new();
    let mut finished = HashSet::new();
    let read = read_until(&mut subscription, |record| {
        match &record.body {
            RecordBody::InsertRemote { key, from, .. }
                if record.store == ObservedStore::Records =>
            {
                carrying.entry(from.clone()).or_default().push(key.clone());
            }
            RecordBody::SyncFinished { peer, ok: true, .. }
                if record.store == ObservedStore::Records =>
            {
                finished.extend(carrying.remove(peer).unwrap_or_default());
            }
            _ => {}
        }
        finished.len() >= HELD
    })
    .await?;
    assert_eq!(finished, keys);
    let seqs: Vec<u64> = read.iter().map(|record| record.seq).collect();
    let expected: Vec<u64> = (0..).take(read.len()).collect();
    assert_eq!(seqs, expected, "no gap from 0");
    assert!(read.iter().all(|record| record.body != RecordBody::Lagged));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// The control of the observed join: a pod of [`CONTROL_HELD`] records
/// joined through `join` and then a subscription holds no `insert_remote` of
/// the records the joiner's first session carried, up to a record placed
/// after the join. Fails with `join` answering before its catch-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_product_join_followed_by_a_subscription_misses_the_first_session() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let (pod, keys, invite) = pod_holding_records(&alice_phone, alice, CONTROL_HELD).await?;

    bob_phone.pods().join(bob, invite).await?;
    let mut subscription = bob_phone.observe_pod_for_test(bob, pod).await?;
    let sentinel = place(&alice_phone, alice, pod, b"after the join").await?;
    let read = read_until(&mut subscription, |record| {
        insert_key(record) == Some(sentinel.as_str())
    })
    .await?;
    assert!(
        read.iter()
            .filter_map(insert_key)
            .all(|key| !keys.contains(key)),
        "an insert of the first session reached a subscription opened after the join"
    );

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An observed join on an invite another identity's observed join
/// consumed is refused as `join` refuses it and opens no subscription: its
/// identity holds no store of the pod. Fails with the invite's secret not
/// burned.
#[tokio::test(flavor = "multi_thread")]
async fn an_observed_join_on_a_consumed_invite_is_refused() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = bob_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    let (joined, _subscription) = bob_phone
        .join_pod_observed_for_test(bob, invite.clone())
        .await?;
    assert_eq!(joined, pod);
    // Denied (the consumed invite).
    let refused = bob_phone.join_pod_observed_for_test(carol, invite).await;
    assert!(refused.is_err_and(|err| is::<JoinRefused>(&err)));
    let observed = bob_phone.observe_pod_for_test(carol, pod).await;
    assert!(observed.is_err_and(|err| is::<UnknownPod>(&err)));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}
