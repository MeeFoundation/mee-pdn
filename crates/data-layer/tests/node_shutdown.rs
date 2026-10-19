//! A node's stop, and its drop: its engines, then its router, then the blob
//! protocol's streams, then its blob store, so neither a payload fetch under
//! way nor a request a blob stream has read meets a blob store closing
//! beneath it, and no blob collection outlives the store (the
//! `node-assembly` spec, "Shutdown stops every protocol side by side, and a
//! late call fails").

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use data_layer::{
    identity_of, BlobStreamHold, Contact, PodStore, SpawnOptions, StopStep, SyncNode,
};
use futures_lite::StreamExt;
use iroh_blobs::protocol::ObserveRequest;
use test_utils::{
    count_panics,
    pod::{create, device_of, host, invite, place_claim, tickets},
};

/// A node stopped while it fetches a payload from a member device stops its
/// engines before its blob store closes, and without a panic on any thread.
///
/// The writer serves no payload, so the reader's fetch fails at the dial and
/// is tried again until the reader stops. The reader runs on a directory:
/// its file-backed blob store is the one that panics on a read reaching it
/// once closed. A fetch lands between the two steps only some of the time,
/// so their order is read from the node's stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_stopped_with_a_fetch_under_way_stops_without_a_panic() -> Result<()> {
    let panics = count_panics();
    let alice_phone = SyncNode::spawn_serving_no_payloads_for_test(SpawnOptions::memory()).await?;
    let dir = tempfile::tempdir()?;
    let bob_phone = SyncNode::spawn(SpawnOptions::on_directory(dir.path())).await?;
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
    place_claim(&alice_phone, &alice, pod, 1).await?;
    let alices = Contact::new(alice_phone.dial_handle().addr(), identity_of(alice.id));
    bob_phone
        .sync_pod_with_for_test(bob.id, pod, PodStore::Records, alices)
        .await?;

    bob_phone.shutdown().await?;
    alice_phone.shutdown().await?;
    let steps = bob_phone
        .stop_steps_for_test()
        .lock()
        .map_err(|_poisoned| anyhow::anyhow!("stop steps poisoned"))?
        .clone();
    let at = |wanted: StopStep| steps.iter().position(|step| *step == wanted);
    assert!(
        at(StopStep::Engines) < at(StopStep::BlobStore) && at(StopStep::Engines).is_some(),
        "the engines must stop before the blob store closes: {steps:?}"
    );
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}

/// Alice's phone on `dir`, holding a claim of a pod of hers, and a blob
/// stream of it held once it has read Bob's phone's observe of a payload it
/// does not hold, so the read loads an entry the store has not loaded.
struct HeldObserve {
    alice_phone: SyncNode,
    bob_phone: SyncNode,
    hold: Arc<BlobStreamHold>,
    observing: tokio::task::JoinHandle<()>,
}

async fn held_observe(dir: &std::path::Path) -> Result<HeldObserve> {
    let alice_phone = SyncNode::spawn(SpawnOptions::on_directory(dir)).await?;
    let bob_phone = SyncNode::spawn(SpawnOptions::memory()).await?;
    let (alice, _) = host(&alice_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    place_claim(&alice_phone, &alice, pod, 1).await?;
    let hold = alice_phone.hold_next_blob_stream_for_test();
    let connection = bob_phone
        .dial_handle()
        .connect(alice_phone.dial_handle().addr(), iroh_blobs::ALPN)
        .await?;
    let observer = iroh_blobs::store::mem::MemStore::new();
    let observing = tokio::spawn(async move {
        let request = ObserveRequest::new(iroh_blobs::Hash::new(b"claim"));
        let mut updates = Box::pin(observer.remote().observe(connection, request));
        let _ = updates.next().await;
    });
    hold.reached.notified().await;
    Ok(HeldObserve {
        alice_phone,
        bob_phone,
        hold,
        observing,
    })
}

/// Releases the held stream, should the stop have left it held, and waits
/// for it to answer: a panic its read raises is counted by then.
async fn answered(held: &HeldObserve) -> Result<()> {
    held.hold.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), held.hold.done.notified())
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the held blob stream never answered"))
}

/// A node stopped while one of its blob streams holds a peer's request stops
/// without a panic on any thread: the stream answers before the blob store
/// closes.
///
/// The stream is held after it reads the request until the router's
/// shutdown has returned, where a store closed at once meets the read.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_stopped_while_a_peer_observes_its_payload_stops_without_a_panic() -> Result<()> {
    let panics = count_panics();
    let dir = tempfile::tempdir()?;
    let held = held_observe(dir.path()).await?;

    held.alice_phone.shutdown().await?;
    answered(&held).await?;
    let _ = tokio::time::timeout(Duration::from_secs(10), held.observing).await;
    held.bob_phone.shutdown().await?;
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}

/// A node dropped without a stop while one of its blob streams holds a
/// peer's request stops without a panic on any thread: the stream answers
/// before the blob store closes, as at a stop.
///
/// The drop stops the node in a task of its own; a node on the same
/// directory spawns once that stop has let go of the directory and of the
/// blob store.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_dropped_while_a_peer_observes_its_payload_stops_without_a_panic() -> Result<()> {
    let panics = count_panics();
    let dir = tempfile::tempdir()?;
    let HeldObserve {
        alice_phone,
        bob_phone,
        hold,
        observing,
    } = held_observe(dir.path()).await?;

    drop(alice_phone);
    let deadline = Instant::now() + Duration::from_secs(20);
    let respawned = loop {
        match SyncNode::spawn(SpawnOptions::on_directory(dir.path())).await {
            Ok(node) => break node,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "the dropped node never let go of its directory: {err:#}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    hold.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), hold.done.notified())
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the held blob stream never answered"))?;
    let _ = tokio::time::timeout(Duration::from_secs(10), observing).await;
    respawned.shutdown().await?;
    bob_phone.shutdown().await?;
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}

/// Every 20 ms, so a collection that outlives its node shows within a
/// fraction of a second.
const COLLECTION_INTERVAL: Duration = Duration::from_millis(20);

/// A node on `dir` hosting an identity, collecting at
/// [`COLLECTION_INTERVAL`], with its count of collection runs once two have
/// run.
async fn collecting_node(dir: &std::path::Path) -> Result<(SyncNode, Arc<AtomicUsize>)> {
    let mut options = SpawnOptions::on_directory(dir);
    options.blob_collection_interval = COLLECTION_INTERVAL;
    let node = SyncNode::spawn(options).await?;
    let _hosted = host(&node).await?;
    node.start_blob_collection();
    let runs = node.collection_runs_for_test();
    let deadline = Instant::now() + Duration::from_secs(10);
    while runs.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "blob collection never ran");
        tokio::time::sleep(COLLECTION_INTERVAL).await;
    }
    Ok((node, runs))
}

/// Waits for `runs` to stand still over ten collection intervals. A run
/// under way at the stop may still count itself, and a dropped node stops
/// in a task of its own, so the count is given a few seconds to settle.
async fn collection_ends(runs: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let before = runs.load(Ordering::SeqCst);
        tokio::time::sleep(COLLECTION_INTERVAL * 10).await;
        if runs.load(Ordering::SeqCst) == before {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "blob collection kept running after its node stopped"
        );
    }
}

/// A stopped node leaves no blob collection running, which would hold the
/// blob store's runtime, one thread per core, for the rest of the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_node_leaves_no_blob_collection_running() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (node, runs) = collecting_node(dir.path()).await?;
    node.shutdown().await?;
    drop(node);
    collection_ends(&runs).await;
    Ok(())
}

/// A node dropped without a stop leaves no blob collection running.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_node_leaves_no_blob_collection_running() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (node, runs) = collecting_node(dir.path()).await?;
    drop(node);
    collection_ends(&runs).await;
    Ok(())
}
