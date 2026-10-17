//! A node's stop: its engines, then its endpoint, then its router, so neither
//! a payload fetch under way nor a peer's request on an open connection meets
//! a blob store closing beneath it (the `node-assembly` spec,
//! "Shutdown stops every protocol side by side, and a late call fails").

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use anyhow::Result;
use data_layer::{identity_of, Contact, PodStore, SpawnOptions, SyncNode};
use futures_lite::StreamExt;
use iroh_blobs::protocol::ObserveRequest;
use test_utils::{
    count_panics,
    pod::{create, device_of, host, invite, place_claim, tickets},
};

/// A node stopped while it fetches a payload from a member device stops
/// without a panic on any thread.
///
/// The writer serves no payload, so the reader's fetch fails at the dial and
/// is tried again until the reader stops. The reader runs on a directory:
/// its file-backed blob store is the one that panics on a read reaching it
/// while it closes, and a running engine starts fetches that read it first.
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
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}

/// A node stopped while a peer observes one of its payloads stops without a
/// panic on any thread.
///
/// The peer asks again as soon as each answer arrives, over a connection
/// opened before the stop, so requests keep arriving while the node stops.
/// Repeated, since a request lands while the blob store closes only some of
/// the time.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_stopped_while_a_peer_observes_its_payload_stops_without_a_panic() -> Result<()> {
    let panics = count_panics();
    let observer = iroh_blobs::store::mem::MemStore::new();
    for _ in 0..10 {
        let dir = tempfile::tempdir()?;
        let alice_phone = SyncNode::spawn(SpawnOptions::on_directory(dir.path())).await?;
        let bob_phone = SyncNode::spawn(SpawnOptions::memory()).await?;
        let (alice, _) = host(&alice_phone).await?;
        let pod = create(&alice_phone, &alice).await?;
        place_claim(&alice_phone, &alice, pod, 1).await?;
        let connection = bob_phone
            .dial_handle()
            .connect(alice_phone.dial_handle().addr(), iroh_blobs::ALPN)
            .await?;
        let answered = Arc::new(AtomicUsize::new(0));
        let observing = {
            let (remote, answered) = (observer.remote().clone(), Arc::clone(&answered));
            tokio::spawn(async move {
                let request = ObserveRequest::new(iroh_blobs::Hash::new(b"claim"));
                loop {
                    let mut updates = Box::pin(remote.observe(connection.clone(), request.clone()));
                    let Some(Ok(_bitfield)) = updates.next().await else {
                        return;
                    };
                    answered.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        while answered.load(Ordering::SeqCst) < 20 && !observing.is_finished() {
            tokio::task::yield_now().await;
        }

        alice_phone.shutdown().await?;
        let _ = observing.await;
        bob_phone.shutdown().await?;
    }
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}
