use std::time::Duration;

use super::*;

type StoreFeed = mpsc::UnboundedSender<anyhow::Result<LiveEvent>>;

fn store() -> (StoreFeed, OrderedEvents) {
    let (feed, mut events) = mpsc::unbounded_channel();
    let events = futures_lite::stream::poll_fn(move |cx| events.poll_recv(cx));
    (feed, Box::pin(events))
}

struct Pod {
    membership: StoreFeed,
    records: StoreFeed,
    subscription: PodSubscription,
}

fn observed(bound: usize) -> Pod {
    let (membership, membership_events) = store();
    let (records, records_events) = store();
    let observation = PodObservation {
        membership: membership_events,
        records: Some(records_events),
    };
    let pod: PodId = "ad58a3faa04cdc5576c8dc5823a347c6"
        .parse()
        .expect("a pod id");
    let subscription = subscribe(
        pod,
        observation,
        CancellationToken::new(),
        bound,
        0x0c41_e0a9_d7f2_2b81,
    );
    Pod {
        membership,
        records,
        subscription,
    }
}

async fn read(subscription: &mut PodSubscription) -> PodRecord {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("a record within the wait")
        .expect("the subscription is open")
}

/// Lets the numbering task take everything its stores hold.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Records of both stores, a store's lag notice and a store's end among
/// them, are numbered 0, 1, 2… under one subscription id, and the other
/// store goes on after the end. Fails with the counter not advancing on a
/// lag notice, or with `closed` not numbered.
#[tokio::test]
async fn records_of_two_stores_are_numbered_without_a_gap() {
    let mut pod = observed(RECORD_BUFFER_BOUND);
    let mut seen = Vec::new();
    pod.membership
        .send(Ok(LiveEvent::PendingContentReady))
        .expect("open");
    seen.push(read(&mut pod.subscription).await);
    pod.records.send(Ok(LiveEvent::Lagged)).expect("open");
    seen.push(read(&mut pod.subscription).await);
    pod.records
        .send(Ok(LiveEvent::PendingContentReady))
        .expect("open");
    seen.push(read(&mut pod.subscription).await);
    drop(pod.records);
    seen.push(read(&mut pod.subscription).await);
    pod.membership
        .send(Ok(LiveEvent::PendingContentReady))
        .expect("open");
    seen.push(read(&mut pod.subscription).await);

    let shape: Vec<_> = seen
        .iter()
        .map(|record| (record.seq, record.store, record.body.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                0,
                ObservedStore::Membership,
                RecordBody::PendingContentReady
            ),
            (1, ObservedStore::Records, RecordBody::Lagged),
            (2, ObservedStore::Records, RecordBody::PendingContentReady),
            (3, ObservedStore::Records, RecordBody::Closed),
            (
                4,
                ObservedStore::Membership,
                RecordBody::PendingContentReady
            ),
        ]
    );
    assert!(seen.iter().all(|record| record.sub == "0c41e0a9d7f22b81"
        && record.sub == pod.subscription.id()
        && record.pod == "ad58a3faa04cdc5576c8dc5823a347c6"));
}

/// A buffer filled past its bound yields `lagged` for the store that lost
/// records once the consumer makes room, with no gap in the numbering; the
/// store that lost nothing is not reported. Fails with a dropped record
/// numbered.
#[tokio::test]
async fn a_full_buffer_yields_lagged_for_the_store_that_lost_records() {
    let mut pod = observed(4);
    for _ in 0..3 {
        pod.membership
            .send(Ok(LiveEvent::PendingContentReady))
            .expect("open");
        pod.records
            .send(Ok(LiveEvent::PendingContentReady))
            .expect("open");
    }
    settle().await;
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(read(&mut pod.subscription).await);
    }
    pod.records
        .send(Ok(LiveEvent::PendingContentReady))
        .expect("open");
    seen.push(read(&mut pod.subscription).await);

    let seqs: Vec<u64> = seen.iter().map(|record| record.seq).collect();
    assert_eq!(seqs, (0..6).collect::<Vec<_>>(), "no gap");
    let lagged: Vec<_> = seen
        .iter()
        .filter(|record| record.body == RecordBody::Lagged)
        .map(|record| record.store)
        .collect();
    assert_eq!(lagged, vec![ObservedStore::Records]);
    let held = seen
        .iter()
        .filter(|record| record.body == RecordBody::PendingContentReady)
        .count();
    assert_eq!(held, 5, "4 that fit, and the one after the lag");
}

/// The one JSON form carries the kind beside the common fields and leaves
/// out what a record does not have.
#[test]
fn a_record_has_one_json_form() {
    let record = PodRecord {
        sub: "0c41e0a9d7f22b81".to_owned(),
        seq: 3,
        t: 1_790_000_044_051_000,
        pod: "ad58a3faa04cdc5576c8dc5823a347c6".to_owned(),
        store: ObservedStore::Records,
        body: RecordBody::SyncFinished {
            peer: "ab".to_owned(),
            origin: SessionOrigin::Accept,
            session: None,
            started: 1,
            finished: 2,
            ok: true,
            error: None,
            sent: Some(12),
            received: Some(0),
        },
    };
    let json = serde_json::to_value(&record).expect("serializes");
    assert_eq!(
        json,
        serde_json::json!({
            "sub": "0c41e0a9d7f22b81", "seq": 3, "t": 1_790_000_044_051_000_u64,
            "pod": "ad58a3faa04cdc5576c8dc5823a347c6", "store": "records",
            "kind": "sync_finished", "peer": "ab", "origin": "accept",
            "started": 1, "finished": 2, "ok": true, "sent": 12, "received": 0,
        })
    );
    let back: PodRecord = serde_json::from_value(json).expect("deserializes");
    assert_eq!(back, record);
}

/// A listed entry's payload is standard base64 in its JSON form, and an
/// absent payload and a `counted` entry's reason are left out.
#[test]
fn a_listed_entry_has_one_json_form() {
    let present = ListedEntry {
        key: "by/w3/immutable-document/a/1".to_owned(),
        author: "ab".to_owned(),
        hash: "8c1d".to_owned(),
        len: 3,
        present: true,
        payload: Some(vec![0xfb, 0xff, 0x00]),
        verdict: "counted".to_owned(),
        reason: None,
    };
    let missing = ListedEntry {
        present: false,
        payload: None,
        verdict: "not_yet".to_owned(),
        reason: Some("payload".to_owned()),
        ..present.clone()
    };
    let json = serde_json::to_value([&present, &missing]).expect("serializes");
    assert_eq!(
        json,
        serde_json::json!([
            {
                "key": "by/w3/immutable-document/a/1", "author": "ab", "hash": "8c1d",
                "len": 3, "present": true, "payload": "+/8A", "verdict": "counted",
            },
            {
                "key": "by/w3/immutable-document/a/1", "author": "ab", "hash": "8c1d",
                "len": 3, "present": false, "verdict": "not_yet", "reason": "payload",
            },
        ])
    );
    let back: Vec<ListedEntry> = serde_json::from_value(json).expect("deserializes");
    assert_eq!(back, [present, missing]);
}

/// A store's end reaches a consumer that let the buffer fill: `closed` is
/// numbered past the bound, behind the `lagged` of what the store lost.
/// Fails with `closed` dropped at the bound like a record.
#[tokio::test]
async fn a_full_buffer_still_yields_the_stores_end() {
    let mut pod = observed(2);
    for _ in 0..4 {
        pod.records
            .send(Ok(LiveEvent::PendingContentReady))
            .expect("open");
    }
    settle().await;
    drop(pod.records);
    settle().await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(read(&mut pod.subscription).await);
    }

    let shape: Vec<_> = seen
        .iter()
        .map(|record| (record.seq, record.store, record.body.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (0, ObservedStore::Records, RecordBody::PendingContentReady),
            (1, ObservedStore::Records, RecordBody::PendingContentReady),
            (2, ObservedStore::Records, RecordBody::Lagged),
            (3, ObservedStore::Records, RecordBody::Closed),
        ]
    );
}
