//! The observation routes across containers: nodes of the observation
//! image, reached over their published HTTP ports, and the stand's image
//! beside them for the routes' absence. Ignored by default: `just
//! test-docker` builds both images and runs the suite.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::{
    observation::{ContentStatusText, ObservedStore, PodRecord, RecordBody},
    PdnId, PodId, RecordKind,
};
use pdn_node_http::shapes::{Act, HeldPods};

mod common;
use common::{pod_route, record_reads, Host, Image, Method, Read, RecordStream, Stand};

/// The wait for one record of a live pod: a write's announcement, or a
/// session it asks for, takes seconds; nothing here waits on the pod pass.
const RECORD_BUDGET: Duration = Duration::from_secs(60);

/// Several times what an announced write takes to reach a member device,
/// which the remaining member's read of the later record has just shown.
const REMOVED_WATCH: Duration = Duration::from_secs(5);

/// The observation routes, by method and path; a hosted identity is not
/// needed, because the gate decides before any handler runs.
const OBSERVATION_ROUTES: &[(Method, &str)] = &[
    (Method::Get, "/debug/identities/aa/pods/bb/events"),
    (Method::Post, "/debug/identities/aa/pods/join-observed"),
    (Method::Get, "/debug/identities/aa/pods/bb/listing"),
];

async fn status_lines(node: &Host) -> Result<Vec<String>> {
    let status = node.get("/debug/status").await?.ok()?;
    Ok(String::from_utf8_lossy(&status)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// The key of the one record `identity`'s listing holds with `payload`.
async fn key_of(node: &Host, identity: PdnId, pod: PodId, payload: &[u8]) -> Result<String> {
    node.listing(identity, pod)
        .await?
        .into_iter()
        .find(|entry| entry.payload.as_deref() == Some(payload))
        .map(|entry| entry.key)
        .context("the writer does not list its own record")
}

fn is_insert_of(record: &PodRecord, wanted: &str) -> bool {
    matches!(&record.body,
        RecordBody::InsertLocal { key, .. } | RecordBody::InsertRemote { key, .. } if key == wanted)
}

fn is_records_session(record: &PodRecord) -> bool {
    record.store == ObservedStore::Records
        && matches!(
            record.body,
            RecordBody::SyncFinished {
                ok: true,
                session: Some(_),
                ..
            }
        )
}

/// `(session, sent, received)` of a record-store `sync_finished`.
fn session_of(record: &PodRecord) -> Option<(String, Option<u64>, Option<u64>)> {
    match &record.body {
        RecordBody::SyncFinished {
            session: Some(session),
            sent,
            received,
            ..
        } if record.store == ObservedStore::Records => Some((session.clone(), *sent, *received)),
        _ => None,
    }
}

fn entry_ts(record: Option<&PodRecord>) -> Option<u64> {
    match record.map(|record| &record.body) {
        Some(
            RecordBody::InsertLocal { entry_ts, .. } | RecordBody::InsertRemote { entry_ts, .. },
        ) => Some(*entry_ts),
        _ => None,
    }
}

/// A record placed on one node is observed on both: the writer's
/// `insert_local` and the reader's `insert_remote` from the writer's node
/// carry one entry timestamp, and a record-store session after it carries
/// one session id and mirrored counts on both sides. The node also states
/// it is a release build with observation. Paired denial: an identity of
/// the reader's node holding no store of the pod is refused the event route
/// and the listing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn a_record_crossing_is_observed_on_both_sides() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand
        .spawn_from("alice", Image::Observation, true, &[])
        .await?;
    let bob_node = stand
        .spawn_from("bob", Image::Observation, true, &[])
        .await?;
    let status = status_lines(&alice_node).await?;
    for line in ["profile release", "observation on"] {
        assert!(status.iter().any(|held| held == line), "{status:?}");
    }
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let bob_work = bob_node.create_identity().await?;
    let pod = alice_node.create_pod(alice).await?;
    let invite = alice_node.invite_to_pod(alice, pod).await?;
    bob_node.join_pod(bob, invite).await?.ok()?;
    let mut written = alice_node.events(alice, pod).await?;
    let mut received = bob_node.events(bob, pod).await?;
    // Denied (co-located, no store of the pod).
    for route in ["events", "listing"] {
        let refused = bob_node
            .get(&format!("{}/{route}", pod_route(bob_work, pod)))
            .await?;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{route}: {}",
            refused.text()
        );
    }

    alice_node
        .place_record(alice, pod, RecordKind::ImmutableDocument, b"crossing")
        .await?;
    let key = key_of(&alice_node, alice, pod, b"crossing").await?;
    let mut on_alice = written
        .read_until(RECORD_BUDGET, "the insert_local", |record| {
            is_insert_of(record, &key)
        })
        .await?;
    let on_bob = received
        .read_until(RECORD_BUDGET, "the insert_remote", |record| {
            is_insert_of(record, &key)
        })
        .await?;
    let alice_id = alice_node.node_id().await?;
    match on_bob.last().map(|record| &record.body) {
        Some(RecordBody::InsertRemote { from, .. }) => assert_eq!(from, &alice_id),
        other => anyhow::bail!("not an insert_remote: {other:?}"),
    }
    assert_eq!(entry_ts(on_alice.last()), entry_ts(on_bob.last()));

    let after = received
        .read_until(RECORD_BUDGET, "a record-store session", is_records_session)
        .await?;
    let (session, sent, got) = after
        .last()
        .and_then(session_of)
        .context("no session read")?;
    if !on_alice
        .iter()
        .any(|record| session_of(record).is_some_and(|(id, ..)| id == session))
    {
        on_alice.extend(
            written
                .read_until(RECORD_BUDGET, "the same session", |record| {
                    session_of(record).is_some_and(|(id, ..)| id == session)
                })
                .await?,
        );
    }
    let mirrored = on_alice
        .iter()
        .filter_map(session_of)
        .find(|(id, ..)| *id == session)
        .map(|(_, alice_sent, alice_got)| (alice_got, alice_sent));
    assert_eq!(mirrored, Some((sent, got)), "session {session}");
    Ok(())
}

/// A member an owner removes sees its record stream close while its event
/// route stays open. Paired denial: no record placed after the removal reaches
/// it, while the remaining member reads that record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn a_removed_members_record_stream_closes() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand
        .spawn_from("alice", Image::Observation, true, &[])
        .await?;
    let bob_node = stand
        .spawn_from("bob", Image::Observation, true, &[])
        .await?;
    let carol_node = stand
        .spawn_from("carol", Image::Observation, true, &[])
        .await?;
    let alice = alice_node.create_identity().await?;
    let bob = bob_node.create_identity().await?;
    let carol = carol_node.create_identity().await?;
    let pod = alice_node.create_pod(alice).await?;
    for (node, identity) in [(&bob_node, bob), (&carol_node, carol)] {
        let invite = alice_node.invite_to_pod(alice, pod).await?;
        node.join_pod(identity, invite).await?.ok()?;
    }
    let mut removed = carol_node.events(carol, pod).await?;

    alice_node
        .pod_act(alice, pod, Act::Remove(carol))
        .await?
        .ok()?;
    removed
        .read_until(RECORD_BUDGET, "the record store's closed", |record| {
            record.store == ObservedStore::Records && record.body == RecordBody::Closed
        })
        .await?;
    let later = alice_node
        .place_record(alice, pod, RecordKind::Claim, b"after the removal")
        .await?;
    let key = key_of(&alice_node, alice, pod, b"after the removal").await?;
    record_reads(&bob_node, bob, pod, later, b"after the removal").await?;

    // Denied (the removaled member): nothing of the record store, and the
    // route still open.
    let watched_until = tokio::time::Instant::now() + REMOVED_WATCH;
    loop {
        let wait = watched_until.saturating_duration_since(tokio::time::Instant::now());
        match removed.read(wait).await? {
            Read::Record(record) => assert!(
                record.store == ObservedStore::Membership && !is_insert_of(&record, &key),
                "the removaled member's route yielded {record:?}"
            ),
            Read::Ended => anyhow::bail!("the removaled member's event route ended"),
            Read::Quiet => break,
        }
    }
    Ok(())
}

/// A pod holding `count` records of distinct payloads of `size` bytes,
/// placed `in_flight` at a time, below the request bound, and their keys.
async fn pod_holding(
    node: &Arc<Host>,
    identity: PdnId,
    count: usize,
    size: usize,
    in_flight: usize,
) -> Result<(PodId, HashSet<String>)> {
    let pod = node.create_pod(identity).await?;
    let mut puts = tokio::task::JoinSet::new();
    for index in 0..count {
        if puts.len() == in_flight {
            puts.join_next().await.context("no put in flight")???;
        }
        let node = Arc::clone(node);
        puts.spawn(async move {
            let mut payload = index.to_le_bytes().to_vec();
            payload.resize(size, b'.');
            node.place_record(identity, pod, RecordKind::ImmutableDocument, &payload)
                .await
        });
    }
    while let Some(put) = puts.join_next().await {
        put??;
    }
    let keys: HashSet<String> = node
        .listing(identity, pod)
        .await?
        .into_iter()
        .map(|entry| entry.key)
        .collect();
    anyhow::ensure!(keys.len() == count, "{} of {count} listed", keys.len());
    Ok((pod, keys))
}

/// Reads an observed join's stream until each of `keys` was carried by a
/// record-store session that finished after it, from `seq` 0 with no loss.
async fn read_catch_up(
    stream: &mut RecordStream,
    keys: &HashSet<String>,
    budget: Duration,
) -> Result<Vec<PodRecord>> {
    // Per sending node, the inserts its next record-store session has yet
    // to finish.
    let mut carrying: HashMap<String, Vec<String>> = HashMap::new();
    let mut finished = HashSet::new();
    let read = stream
        .read_until(budget, "every record's session", |record| {
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
                // Ends the wait, for the check below to fail on.
                RecordBody::Lagged => return true,
                _ => {}
            }
            finished.len() >= keys.len()
        })
        .await?;
    assert_from_zero_without_loss(&read);
    assert_eq!(&finished, keys);
    Ok(read)
}

fn assert_from_zero_without_loss(read: &[PodRecord]) {
    let seqs: Vec<u64> = read.iter().map(|record| record.seq).collect();
    let expected: Vec<u64> = (0..).take(read.len()).collect();
    assert_eq!(seqs, expected, "no gap from 0");
    assert!(read.iter().all(|record| record.body != RecordBody::Lagged));
}

/// A member joining through the observed join route receives an
/// `insert_remote` of every record the pod held before the record-store
/// `sync_finished` of the session that carried it, from `seq` 0. Paired
/// denial: an observed join on the consumed invite is answered the join
/// route's refusal, with no stream and no pod.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn an_observed_join_streams_the_catch_up_from_its_first_insert() -> Result<()> {
    let stand = Stand::new();
    let alice_node = Arc::new(
        stand
            .spawn_from("alice", Image::Observation, true, &[])
            .await?,
    );
    let dave_node = stand
        .spawn_from("dave", Image::Observation, true, &[])
        .await?;
    let alice = alice_node.create_identity().await?;
    let dave = dave_node.create_identity().await?;
    let dave_work = dave_node.create_identity().await?;
    let (pod, keys) = pod_holding(&alice_node, alice, 200, 64, 8).await?;
    let invite = alice_node.invite_to_pod(alice, pod).await?;

    let mut joined = dave_node
        .join_pod_observed(dave, invite.clone())
        .await?
        .map_err(|refused| anyhow::anyhow!("{}: {}", refused.status, refused.text()))?;
    assert_eq!(joined.pod, Some(pod.to_string()));
    read_catch_up(&mut joined, &keys, RECORD_BUDGET).await?;

    // Denied (the consumed invite).
    let refused = dave_node
        .join_pod_observed(dave_work, invite.clone())
        .await?
        .err()
        .context("an observed join on a consumed invite streamed")?;
    let product = dave_node.join_pod(dave_work, invite).await?;
    assert_eq!(
        (refused.status, refused.text()),
        (product.status, product.text())
    );
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let held: HeldPods = dave_node
        .get(&format!("/debug/identities/{dave_work}/pods"))
        .await?
        .json()?;
    assert!(held.pods.is_empty(), "{held:?}");
    Ok(())
}

/// The observation routes are absent from the stand's image with the debug
/// surface on, whose status states `observation off`, and from the
/// observation image without it. Paired: the observation image with the
/// debug surface serves every one of them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn the_observation_routes_exist_only_in_the_observation_build_with_the_flag() -> Result<()> {
    let stand = Stand::new();
    let product = stand.spawn_from("product", Image::Stand, true, &[]).await?;
    let gated = stand
        .spawn_from("gated", Image::Observation, false, &[])
        .await?;
    let observed = stand
        .spawn_from("observed", Image::Observation, true, &[])
        .await?;

    let status = status_lines(&product).await?;
    for line in ["profile release", "observation off"] {
        assert!(status.iter().any(|held| held == line), "{status:?}");
    }
    for (method, path) in OBSERVATION_ROUTES {
        // Denied (the stand's image, and the observation image without the
        // flag).
        for node in [&product, &gated] {
            let answer = node.request(*method, path, Bytes::new()).await?;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method:?} {path}");
        }
        let answer = observed.request(*method, path, Bytes::new()).await?;
        assert_ne!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method:?} {path} must exist: {}",
            answer.text()
        );
    }
    Ok(())
}

/// A pod interval that is no positive whole number stops the start, the
/// log naming the variable. Paired: a valid one starts a node that answers
/// liveness.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn an_invalid_pod_interval_stops_the_start() -> Result<()> {
    const VARIABLE: &str = "PDN_POD_RECONCILE_INTERVAL_SECS";
    let stand = Stand::new();
    let (code, log) = stand.refused_start("refused", &[(VARIABLE, "0")]).await?;
    assert_ne!(code, 0, "{log}");
    assert!(log.contains(VARIABLE), "{log}");

    stand
        .spawn_from("started", Image::Stand, true, &[(VARIABLE, "30")])
        .await?;
    Ok(())
}

/// The largest catch-up a join survives: an observed join of a pod holding
/// 1,000 records of 256 B yields every record's `insert_remote` and its
/// content, from `seq` 0 with no `lagged`, although the route answers only
/// once the join has waited and its consumer reads nothing before. Sized
/// under the live actor's 1,024-event `replica_events` channel: a join
/// session inserting more can deadlock the live actor and the store actor
/// (tasks.md 2.10). At this size the transport between node and test holds
/// the whole stream, so the runtime's draining task is proven in `pdn-node`
/// by `a_consumer_that_reads_late_receives_the_time_of_production`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http images (just test-docker)"]
async fn the_largest_catch_up_is_observed_whole() -> Result<()> {
    const RECORDS: usize = 1_000;
    let stand = Stand::new();
    let alice_node = Arc::new(
        stand
            .spawn_from("alice", Image::Observation, true, &[])
            .await?,
    );
    let dave_node = stand
        .spawn_from("dave", Image::Observation, true, &[])
        .await?;
    let alice = alice_node.create_identity().await?;
    let dave = dave_node.create_identity().await?;
    let (pod, keys) = pod_holding(&alice_node, alice, RECORDS, 256, 12).await?;
    let invite = alice_node.invite_to_pod(alice, pod).await?;

    let mut joined = dave_node
        .join_pod_observed(dave, invite)
        .await?
        .map_err(|refused| anyhow::anyhow!("{}: {}", refused.status, refused.text()))?;
    let mut read = read_catch_up(&mut joined, &keys, Duration::from_mins(10)).await?;
    let mut missing: HashSet<String> = read
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::InsertRemote {
                hash,
                content_status,
                ..
            } if *content_status != ContentStatusText::Complete => Some(hash.clone()),
            _ => None,
        })
        .collect();
    for record in &read {
        if let RecordBody::ContentReady { hash } = &record.body {
            missing.remove(hash);
        }
    }
    if !missing.is_empty() {
        read.extend(
            joined
                .read_until(Duration::from_mins(5), "every record's content", |record| {
                    if let RecordBody::ContentReady { hash } = &record.body {
                        missing.remove(hash);
                    }
                    missing.is_empty()
                })
                .await?,
        );
    }
    assert_from_zero_without_loss(&read);
    Ok(())
}
