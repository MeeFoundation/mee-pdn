use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointAddr, PublicKey, SecretKey, TransportAddr,
};
use iroh_blobs::Hash;
use iroh_gossip::{api::Event as GossipEvent, net::Gossip, proto::TopicId};
use n0_future::{
    time::{Duration, Instant},
    Stream, TryStreamExt,
};
use pdn_store::{
    api::{AddrInfoOptions, Doc},
    engine::{LiveEvent, Origin, SyncEvent, SyncReason},
    store::Query,
    AuthorId, ContentStatus, Entry, EntryFilter, NamespaceId, RestsOn, SessionAccess,
    SessionAccessProvider, SessionRole,
};
use rand::{CryptoRng, RngExt, SeedableRng};
use tempfile::tempdir;
use tokio::sync::Notify;
use tracing::{debug, error_span, info, Instrument};
use tracing_test::traced_test;
mod util;
use util::{Builder, Node};

use crate::util::empty_endpoint;

const TIMEOUT: Duration = Duration::from_secs(60);

async fn test_node(secret_key: SecretKey) -> Result<Builder> {
    let ep = Endpoint::builder(presets::Minimal)
        .secret_key(secret_key)
        .bind()
        .await?;
    Ok(Node::memory(ep))
}

// The function is not `async fn` so that we can take a `&mut` borrow on the `rng` without
// capturing that `&mut` lifetime in the returned future. This allows to call it in a loop while
// still collecting the futures before awaiting them altogether (see [`spawn_nodes`])
fn spawn_node(
    i: usize,
    rng: &mut impl CryptoRng,
) -> impl Future<Output = anyhow::Result<Node>> + 'static {
    let secret_key = SecretKey::from_bytes(&rng.random());
    async move {
        let node = test_node(secret_key).await?;
        let node = node.spawn().await?;
        info!(?i, me = %node.id().fmt_short(), "node spawned");
        Ok(node)
    }
}

async fn spawn_nodes(n: usize, mut rng: &mut impl CryptoRng) -> anyhow::Result<Vec<Node>> {
    let mut futs = vec![];
    for i in 0..n {
        futs.push(spawn_node(i, &mut rng));
    }
    futures_buffered::join_all(futs).await.into_iter().collect()
}

pub fn test_rng(seed: &[u8]) -> rand::rngs::ChaCha12Rng {
    rand::rngs::ChaCha12Rng::from_seed(*Hash::new(seed).as_bytes())
}

macro_rules! match_event {
    ($pattern:pat $(if $guard:expr)? $(,)?) => {
        Box::new(move |e| matches!(e, $pattern $(if $guard)?))
    };
}

/// This tests the simplest scenario: A node connects to another node, and performs sync.
#[tokio::test]
#[traced_test]
async fn sync_simple() -> Result<()> {
    let mut rng = test_rng(b"sync_simple");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    // create doc on node0
    let peer0 = nodes[0].id();
    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let blobs0 = clients[0].blobs();
    let hash0 = doc0
        .set_bytes(author0, b"k1".to_vec(), b"v1".to_vec())
        .await?;
    assert_latest(blobs0, &doc0, b"k1", b"v1").await;
    let ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;

    let mut events0 = doc0.subscribe().await?;

    info!("node1: join");
    let peer1 = nodes[1].id();
    // Subscribed before the sync starts: the neighbor can come up before a
    // subscription made after the import.
    let (doc1, mut events1) = util::import_and_subscribe(clients[1].docs(), ticket.clone()).await?;
    let blobs1 = clients[1].blobs();
    info!("node1: assert 5 events");
    assert_next_unordered(
        &mut events1,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer0)),
            Box::new(move |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == peer0 )),
            Box::new(move |e| match_sync_finished(e, peer0)),
            Box::new(move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == hash0)),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;
    assert_latest(blobs1, &doc1, b"k1", b"v1").await;

    info!("node0: assert 2 events");
    assert_next_unordered(
        &mut events0,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer1)),
            Box::new(move |e| match_sync_finished(e, peer1)),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;

    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// One undecodable gossip message does not end a namespace's receive loop:
/// what comes after it is still delivered, and the next write still arrives
/// live. The sender is a bare gossip peer, not a docs node — the topic id is
/// the namespace id, which any ticket carries, so this is what a ticket holder
/// can do with the ticket alone.
///
/// Ordering: the sender leaves the topic right after the message, and its
/// `NeighborDown` at the subscriber travels the same connection behind the
/// message, so asserting it before the write proves the loop survived the
/// message rather than never having seen it.
#[tokio::test]
#[traced_test]
async fn sync_continues_after_invalid_gossip_message() -> Result<()> {
    let mut rng = test_rng(b"sync_continues_after_invalid_gossip_message");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let hash_k1 = doc0
        .set_bytes(author0, b"k1".to_vec(), b"v1".to_vec())
        .await?;
    let writer_ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;

    let doc1 = util::import(clients[1].docs(), writer_ticket.clone()).await?;
    let mut events1 = doc1.subscribe().await?;
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash_k1),
    )
    .await;

    // The subscriber's own ticket carries its address; the sender bootstraps off both nodes.
    let subscriber_ticket = doc1.share(AddrInfoOptions::RelayAndAddresses).await?;
    let namespace = writer_ticket.capability.id();
    let sender_endpoint = Endpoint::builder(presets::Minimal).bind().await?;
    let lookup = MemoryLookup::new();
    sender_endpoint.address_lookup()?.add(lookup.clone());
    let mut bootstrap = Vec::new();
    for addr in writer_ticket
        .nodes
        .iter()
        .chain(subscriber_ticket.nodes.iter())
    {
        bootstrap.push(addr.id);
        if !addr.is_empty() {
            lookup.add_endpoint_info(addr.clone());
        }
    }
    let sender_gossip = Gossip::builder().spawn(sender_endpoint.clone());
    let sender_router = Router::builder(sender_endpoint.clone())
        .accept(iroh_gossip::ALPN, sender_gossip.clone())
        .spawn();
    let topic = sender_gossip
        .subscribe(TopicId::from(namespace.to_bytes()), bootstrap)
        .await?;
    let (sender, mut sender_events) = topic.split();

    // The subscriber adds the sender to its neighbors before replying, so once the sender sees
    // the subscriber as a neighbor the link is up on both sides.
    let subscriber = nodes[1].id();
    loop {
        let event = sender_events
            .try_next()
            .await?
            .context("gossip stream ended")?;
        if matches!(event, GossipEvent::NeighborUp(peer) if peer == subscriber) {
            break;
        }
    }
    // Neighbors scope, as the engine's own ops: a swarm-scope message is forwarded, and on a
    // triangle the duplicate prunes the writer–subscriber link, which no neighbors-scope
    // report crosses again.
    sender
        .broadcast_neighbors(Bytes::from_static(b"not a postcard-encoded Op"))
        .await?;
    // Both halves gone: the sender quits the topic and sends its neighbors a disconnect.
    drop((sender, sender_events));

    let sender_id = sender_endpoint.id();
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::NeighborDown(peer) if *peer == sender_id),
    )
    .await;

    let hash_k2 = doc0
        .set_bytes(author0, b"k2".to_vec(), b"v2".to_vec())
        .await?;
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash_k2),
    )
    .await;

    sender_router.shutdown().await?;
    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// An entry broadcast on the replica's gossip topic is dropped, never
/// inserted, while the replica keeps converging over the reconciliation an
/// announcement triggers.
///
/// The broadcast entry is valid for the namespace — signed with its secret,
/// by an author the writer never used — so only the rule that content does
/// not ride the topic keeps it out; a write announced afterwards orders the
/// absence, and no node holds the entry for a session to carry.
#[tokio::test]
#[traced_test]
async fn an_entry_broadcast_on_the_topic_is_dropped() -> Result<()> {
    let mut rng = test_rng(b"an_entry_broadcast_on_the_topic_is_dropped");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let hash_k1 = doc0
        .set_bytes(author0, b"k1".to_vec(), b"v1".to_vec())
        .await?;
    let writer_ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;

    let doc1 = util::import(clients[1].docs(), writer_ticket.clone()).await?;
    let mut events1 = doc1.subscribe().await?;
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash_k1),
    )
    .await;

    let subscriber_ticket = doc1.share(AddrInfoOptions::RelayAndAddresses).await?;
    let namespace = writer_ticket.capability.id();
    let sender_endpoint = Endpoint::builder(presets::Minimal).bind().await?;
    let lookup = MemoryLookup::new();
    sender_endpoint.address_lookup()?.add(lookup.clone());
    let mut bootstrap = Vec::new();
    for addr in writer_ticket
        .nodes
        .iter()
        .chain(subscriber_ticket.nodes.iter())
    {
        bootstrap.push(addr.id);
        if !addr.is_empty() {
            lookup.add_endpoint_info(addr.clone());
        }
    }
    let sender_gossip = Gossip::builder().spawn(sender_endpoint.clone());
    let sender_router = Router::builder(sender_endpoint.clone())
        .accept(iroh_gossip::ALPN, sender_gossip.clone())
        .spawn();
    let topic = sender_gossip
        .subscribe(TopicId::from(namespace.to_bytes()), bootstrap)
        .await?;
    let (sender, mut sender_events) = topic.split();
    let subscriber = nodes[1].id();
    loop {
        let event = sender_events
            .try_next()
            .await?
            .context("gossip stream ended")?;
        if matches!(event, GossipEvent::NeighborUp(peer) if peer == subscriber) {
            break;
        }
    }

    let pdn_store::Capability::Write(secret) = &writer_ticket.capability;
    let pusher = pdn_store::Author::new(&mut rng);
    let pushed = pdn_store::SignedEntry::from_parts(
        secret,
        &pusher,
        b"pushed",
        pdn_store::Record::new_current(Hash::new(b"pushed"), 6),
    );
    /// The engine's gossip message as it reads on the wire: postcard numbers
    /// the variants, and `Put` is the first.
    #[derive(serde::Serialize)]
    enum Op {
        Put(pdn_store::SignedEntry),
    }
    sender
        .broadcast_neighbors(Bytes::from(postcard::to_stdvec(&Op::Put(pushed))?))
        .await?;
    drop((sender, sender_events));
    let sender_id = sender_endpoint.id();
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::NeighborDown(peer) if *peer == sender_id),
    )
    .await;

    let hash_k2 = doc0
        .set_bytes(author0, b"k2".to_vec(), b"v2".to_vec())
        .await?;
    next_event_matching(
        &mut events1,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash_k2),
    )
    .await;
    assert!(
        get_exact(&doc1, pusher.id(), b"pushed", true)
            .await?
            .is_none(),
        "an entry broadcast on the topic was inserted"
    );

    sender_router.shutdown().await?;
    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// Test subscribing to replica events (without sync)
#[tokio::test]
#[traced_test]
async fn sync_subscribe_no_sync() -> Result<()> {
    let mut rng = test_rng(b"sync_subscribe");
    let node = spawn_node(0, &mut rng).await?;
    let client = node.client();
    let doc = client.docs().create().await?;
    let mut sub = doc.subscribe().await?;
    let author = client.docs().author_create().await?;
    doc.set_bytes(author, b"k".to_vec(), b"v".to_vec()).await?;
    let event = n0_future::time::timeout(Duration::from_millis(100), sub.try_next()).await?;
    assert!(
        matches!(event, Ok(Some(LiveEvent::InsertLocal { .. }))),
        "expected InsertLocal but got {event:?}"
    );
    node.shutdown().await?;
    Ok(())
}

/// A replica imported without peers takes the 100 entries written before
/// its first session from that session, and 100 more written afterwards as
/// they are written; `N_ENTRIES` sets the count.
#[tokio::test]
#[traced_test]
async fn sync_gossip_bulk() -> Result<()> {
    let n_entries: usize = std::env::var("N_ENTRIES")
        .map(|x| x.parse().expect("N_ENTRIES must be a number"))
        .unwrap_or(100);
    let mut rng = test_rng(b"sync_gossip_bulk");

    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    let _peer0 = nodes[0].id();
    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let mut ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;
    // unset peers to not yet start sync
    let peers = ticket.nodes.clone();
    ticket.nodes = vec![];
    let doc1 = util::import(clients[1].docs(), ticket).await?;
    let mut events = doc1.subscribe().await?;

    // create entries for initial sync.
    let now = Instant::now();
    let value = b"foo";
    for i in 0..n_entries {
        let key = format!("init/{i}");
        doc0.set_bytes(author0, key.as_bytes().to_vec(), value.to_vec())
            .await?;
    }
    let elapsed = now.elapsed();
    info!(
        "insert took {elapsed:?} for {n_entries} ({:?} per entry)",
        elapsed / n_entries as u32
    );

    let now = Instant::now();
    let mut count = 0;
    doc0.start_sync(vec![], util::TEST_HOLDER).await?;
    doc1.start_sync(util::contacts(peers), util::TEST_HOLDER)
        .await?;
    while let Ok(event) = events.try_next().await {
        let event = event.unwrap();
        if matches!(event, LiveEvent::InsertRemote { .. }) {
            count += 1;
        }
        if count == n_entries {
            break;
        }
    }
    let elapsed = now.elapsed();
    info!(
        "initial sync took {elapsed:?} for {n_entries} ({:?} per entry)",
        elapsed / n_entries as u32
    );

    // publish another 1000 entries
    let mut count = 0;
    let value = b"foo";
    let now = Instant::now();
    for i in 0..n_entries {
        let key = format!("gossip/{i}");
        doc0.set_bytes(author0, key.as_bytes().to_vec(), value.to_vec())
            .await?;
    }
    let elapsed = now.elapsed();
    info!(
        "insert took {elapsed:?} for {n_entries} ({:?} per entry)",
        elapsed / n_entries as u32
    );

    while let Ok(event) = events.try_next().await {
        let event = event.unwrap();
        if matches!(event, LiveEvent::InsertRemote { .. }) {
            count += 1;
        }
        if count == n_entries {
            break;
        }
    }
    let elapsed = now.elapsed();
    info!(
        "gossip recv took {elapsed:?} for {n_entries} ({:?} per entry)",
        elapsed / n_entries as u32
    );

    Ok(())
}

/// Requests past a hundred toward one peer whose connect gets no answer all
/// end within a bound, through a connection pool as the engine fetches
/// through one (iroh-util-001).
#[tokio::test(flavor = "multi_thread")]
async fn connections_past_a_hundred_toward_a_silent_peer_all_end() -> Result<()> {
    const REQUESTS: usize = 500;
    // A socket nobody reads: the handshake toward it gets no answer.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let peer = SecretKey::from_bytes(&[7; 32]).public();
    let lookup = MemoryLookup::new();
    lookup.add_endpoint_info(EndpointAddr {
        id: peer,
        addrs: [TransportAddr::Ip(silent.local_addr()?)]
            .into_iter()
            .collect(),
    });
    let endpoint = Endpoint::builder(presets::Minimal)
        .address_lookup(lookup)
        .bind()
        .await?;
    let pool = iroh_blobs::util::connection_pool::ConnectionPool::new(
        endpoint.clone(),
        iroh_blobs::ALPN,
        iroh_blobs::util::connection_pool::Options::default(),
    );

    let ended = Arc::new(AtomicUsize::new(0));
    let requests = (0..REQUESTS).map(|_| {
        let pool = pool.clone();
        let ended = Arc::clone(&ended);
        async move {
            let _failed = pool.get_or_connect(peer).await.is_err();
            ended.fetch_add(1, Ordering::SeqCst);
        }
    });
    // Each fails once the connect times out, 1 s by the pool's default.
    let _bound = tokio::time::timeout(
        Duration::from_secs(10),
        futures_buffered::join_all(requests),
    )
    .await;
    assert_eq!(
        ended.load(Ordering::SeqCst),
        REQUESTS,
        "requests that ended within 10 s"
    );
    endpoint.close().await;
    Ok(())
}

/// A node on loopback with a fresh key, `configure` adjusting its builder.
async fn loopback_node(
    rng: &mut impl CryptoRng,
    configure: impl FnOnce(Builder) -> Builder,
) -> Result<Node> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&rng.random()))
        .bind_addr("127.0.0.1:0")?
        .bind()
        .await?;
    configure(Node::memory(endpoint)).spawn().await
}

/// A test node's blob serving keeps every request unanswered while held,
/// answers them once released, and records how many it held open at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_test_nodes_blob_serving_counts_and_holds_requests() -> Result<()> {
    let mut rng = test_rng(b"a_test_nodes_blob_serving_counts_and_holds_requests");
    let serving = Arc::new(util::BlobServing::default());
    let provider = loopback_node(&mut rng, |node| node.blob_serving(Arc::clone(&serving))).await?;
    let fetcher = loopback_node(&mut rng, |node| node).await?;
    let mut hashes = Vec::new();
    for payload in [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()] {
        hashes.push(provider.blobs().add_bytes(payload).await?.hash);
    }
    serving.hold();
    let connection = fetcher
        .endpoint()
        .connect(provider.endpoint().addr(), iroh_blobs::ALPN)
        .await?;
    let fetches = futures_buffered::join_all(hashes.iter().map(|hash| {
        std::future::IntoFuture::into_future(
            fetcher.blobs().remote().fetch(connection.clone(), *hash),
        )
    }));
    tokio::pin!(fetches);

    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut fetches)
            .await
            .is_err(),
        "a request was answered while held"
    );
    assert_eq!(serving.most_open(), 3, "requests held open at once");
    serving.release();
    let fetched = tokio::time::timeout(TIMEOUT, fetches).await?;
    assert!(
        fetched.iter().all(Result::is_ok),
        "a released request failed: {fetched:?}"
    );

    provider.shutdown().await?;
    fetcher.shutdown().await?;
    Ok(())
}

/// A replica whose payloads went first to a provider that accepts every
/// request and answers none receives them from a second provider of the
/// same payloads.
///
/// The replica joins through the silent provider, so the session that
/// brings the entries makes it every payload's first provider; a session
/// with the second provider follows once every entry is in.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_providers_payloads_come_from_another_provider() -> Result<()> {
    const ENTRIES: usize = 5;
    let mut rng = test_rng(b"a_silent_providers_payloads_come_from_another_provider");
    let silent_serving = Arc::new(util::BlobServing::default());
    let silent = loopback_node(&mut rng, |node| {
        node.blob_serving(Arc::clone(&silent_serving))
    })
    .await?;
    let second = loopback_node(&mut rng, |node| node).await?;
    let joining = loopback_node(&mut rng, |node| node).await?;

    let author = silent.docs().author_create().await?;
    let doc_silent = silent.docs().create().await?;
    let mut hashes = Vec::new();
    for i in 0..ENTRIES {
        hashes.push(
            doc_silent
                .set_bytes(
                    author,
                    format!("record/{i}").into_bytes(),
                    format!("payload {i}").into_bytes(),
                )
                .await?,
        );
    }
    let ticket = doc_silent.share(AddrInfoOptions::RelayAndAddresses).await?;
    let (_doc_second, events_second) =
        util::import_and_subscribe(second.docs(), ticket.clone()).await?;
    wait_for_events(events_second, ENTRIES, TIMEOUT, |e| {
        matches!(e, LiveEvent::ContentReady { .. })
    })
    .await?;
    silent_serving.hold();

    let (doc_joining, mut events) = util::import_and_subscribe(joining.docs(), ticket).await?;
    let (mut inserted, mut ready) = (0, 0);
    let arrived = tokio::time::timeout(TIMEOUT, async {
        while ready < ENTRIES {
            match events.try_next().await? {
                Some(LiveEvent::InsertRemote { .. }) => {
                    inserted += 1;
                    if inserted == ENTRIES {
                        doc_joining
                            .start_sync(
                                util::contacts([second.endpoint().addr()]),
                                util::TEST_HOLDER,
                            )
                            .await?;
                    }
                }
                Some(LiveEvent::ContentReady { .. }) => ready += 1,
                Some(_) => {}
                None => bail!("the event stream ended"),
            }
        }
        anyhow::Ok(())
    })
    .await;
    let fetches = joining.engine().fetches_report().await?;
    assert!(
        matches!(arrived, Ok(Ok(()))),
        "{ready} of {ENTRIES} payloads arrived: {arrived:?}; fetches: {fetches}"
    );
    for hash in hashes {
        assert!(joining.blobs().has(hash).await?, "a payload is missing");
    }

    silent_serving.release();
    for node in [silent, second, joining] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Writes `count` records on `node`, each with a payload of its own,
/// answering the replica and the payloads' hashes.
async fn records_with_payloads(node: &Node, count: usize) -> Result<(Doc, Vec<Hash>)> {
    let author = node.docs().author_create().await?;
    let doc = node.docs().create().await?;
    let mut hashes = Vec::with_capacity(count);
    for i in 0..count {
        hashes.push(
            doc.set_bytes(
                author,
                format!("record/{i}").into_bytes(),
                format!("payload {i}").into_bytes(),
            )
            .await?,
        );
    }
    Ok((doc, hashes))
}

/// Holds `serving`'s requests long enough for every request it answered to
/// be done, and answers how many it holds open then: the requests of its
/// fetchers in flight.
async fn open_while_held(serving: &util::BlobServing) -> usize {
    serving.hold();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let open = serving.open();
    serving.release();
    open
}

/// A replica joining a namespace of 2,000 entries whose payloads one
/// provider holds never has more requests open at that provider than its
/// limit, and receives every payload.
///
/// The count is read with the provider holding every request: a request it
/// has answered can still be counted for a moment after the replica's next
/// one arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_keeps_the_limit_toward_its_provider() -> Result<()> {
    const ENTRIES: usize = 2_000;
    const LIMIT: u32 = 2;
    let mut rng = test_rng(b"a_catch_up_keeps_the_limit_toward_its_provider");
    let serving = Arc::new(util::BlobServing::default());
    let provider = loopback_node(&mut rng, |node| node.blob_serving(Arc::clone(&serving))).await?;
    let joining = loopback_node(&mut rng, |node| node.fetches_per_provider(LIMIT)).await?;
    let (doc, _hashes) = records_with_payloads(&provider, ENTRIES).await?;
    let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;

    let (_doc_joining, mut events) = util::import_and_subscribe(joining.docs(), ticket).await?;
    let mut ready = 0;
    let mut open_counts = Vec::new();
    let arrived = tokio::time::timeout(TIMEOUT, async {
        while ready < ENTRIES {
            match events.try_next().await? {
                Some(LiveEvent::ContentReady { .. }) => {
                    ready += 1;
                    if ready % 500 == 1 {
                        open_counts.push(open_while_held(&serving).await);
                    }
                }
                Some(_) => {}
                None => bail!("the event stream ended"),
            }
        }
        anyhow::Ok(())
    })
    .await;
    let fetches = joining.engine().fetches_report().await?;
    assert!(
        matches!(arrived, Ok(Ok(()))),
        "{ready} of {ENTRIES} payloads arrived: {arrived:?}; fetches: {fetches}"
    );
    assert_eq!(
        open_counts,
        [2, 2, 2, 2],
        "requests open at the provider while it held them"
    );

    provider.shutdown().await?;
    joining.shutdown().await?;
    Ok(())
}

/// A provider that finishes a session with a replica during its catch-up
/// takes payloads the replica still waits for, at most the limit toward it
/// as toward the first provider, and every payload arrives.
///
/// Counts are read with both providers holding every request, as in
/// `a_catch_up_keeps_the_limit_toward_its_provider`.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_registered_during_a_catch_up_takes_waiting_payloads() -> Result<()> {
    const ENTRIES: usize = 2_000;
    const LIMIT: u32 = 10;
    let mut rng = test_rng(b"a_provider_registered_during_a_catch_up_takes_waiting_payloads");
    let (first_serving, second_serving) = (
        Arc::new(util::BlobServing::default()),
        Arc::new(util::BlobServing::default()),
    );
    let first = loopback_node(&mut rng, |node| {
        node.blob_serving(Arc::clone(&first_serving))
    })
    .await?;
    let second = loopback_node(&mut rng, |node| {
        node.blob_serving(Arc::clone(&second_serving))
    })
    .await?;
    let joining = loopback_node(&mut rng, |node| node.fetches_per_provider(LIMIT)).await?;
    let (doc, _hashes) = records_with_payloads(&first, ENTRIES).await?;
    let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
    let (_doc_second, events_second) =
        util::import_and_subscribe(second.docs(), ticket.clone()).await?;
    wait_for_events(events_second, ENTRIES, TIMEOUT, |e| {
        matches!(e, LiveEvent::ContentReady { .. })
    })
    .await?;

    let limit = usize::try_from(LIMIT)?;
    let (doc_joining, mut events) = util::import_and_subscribe(joining.docs(), ticket).await?;
    let mut ready = 0;
    let mut open_counts = Vec::new();
    let arrived = tokio::time::timeout(TIMEOUT, async {
        while ready < ENTRIES {
            match events.try_next().await? {
                Some(LiveEvent::ContentReady { .. }) => {
                    ready += 1;
                    if ready == 1 {
                        doc_joining
                            .start_sync(
                                util::contacts([second.endpoint().addr()]),
                                util::TEST_HOLDER,
                            )
                            .await?;
                    }
                    if ready % 500 == 0 {
                        first_serving.hold();
                        second_serving.hold();
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        open_counts.push((first_serving.open(), second_serving.open()));
                        first_serving.release();
                        second_serving.release();
                    }
                }
                Some(_) => {}
                None => bail!("the event stream ended"),
            }
        }
        anyhow::Ok(())
    })
    .await;
    let fetches = joining.engine().fetches_report().await?;
    assert!(
        matches!(arrived, Ok(Ok(()))),
        "{ready} of {ENTRIES} payloads arrived: {arrived:?}; fetches: {fetches}"
    );
    assert!(
        open_counts
            .iter()
            .all(|(first, second)| *first <= limit && *second <= limit),
        "a provider held more than the limit: {open_counts:?}"
    );
    assert!(
        open_counts.iter().any(|(_, second)| *second > 0),
        "the second provider served nothing: {open_counts:?}"
    );

    for node in [first, second, joining] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A replica whose only provider answers nothing for 15 seconds and then
/// serves receives every payload once it serves, with no session between.
#[tokio::test(flavor = "multi_thread")]
async fn the_only_provider_silent_for_a_while_is_asked_again() -> Result<()> {
    const ENTRIES: usize = 5;
    let mut rng = test_rng(b"the_only_provider_silent_for_a_while_is_asked_again");
    let serving = Arc::new(util::BlobServing::default());
    let provider = loopback_node(&mut rng, |node| node.blob_serving(Arc::clone(&serving))).await?;
    let joining = loopback_node(&mut rng, |node| node).await?;
    let (doc, _hashes) = records_with_payloads(&provider, ENTRIES).await?;
    let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
    serving.hold();

    let (_doc_joining, mut events) = util::import_and_subscribe(joining.docs(), ticket).await?;
    let (mut inserted, mut ready, mut sessions_after_entries) = (0, 0, 0);
    let mut released = false;
    let entries_in = tokio::time::timeout(TIMEOUT, async {
        while inserted < ENTRIES {
            if let Some(LiveEvent::InsertRemote { .. }) = events.try_next().await? {
                inserted += 1;
            }
        }
        anyhow::Ok(())
    })
    .await;
    assert!(
        matches!(entries_in, Ok(Ok(()))),
        "the entries did not arrive"
    );
    // The session that brought the entries ends within a moment of them.
    let quiet_from = Instant::now() + Duration::from_secs(2);
    let silent_until = quiet_from + Duration::from_secs(13);
    let arrived = tokio::time::timeout(TIMEOUT, async {
        while ready < ENTRIES {
            let next = if released {
                events.try_next().await?
            } else {
                match tokio::time::timeout_at(silent_until, events.try_next()).await {
                    Ok(next) => next?,
                    Err(_) => {
                        serving.release();
                        released = true;
                        continue;
                    }
                }
            };
            match next {
                Some(LiveEvent::ContentReady { .. }) => {
                    assert!(released, "a payload arrived while the provider was silent");
                    ready += 1;
                }
                Some(LiveEvent::SyncFinished(_)) if Instant::now() >= quiet_from => {
                    sessions_after_entries += 1;
                }
                Some(_) => {}
                None => bail!("the event stream ended"),
            }
        }
        anyhow::Ok(())
    })
    .await;
    let fetches = joining.engine().fetches_report().await?;
    assert!(
        matches!(arrived, Ok(Ok(()))),
        "{ready} of {ENTRIES} payloads arrived: {arrived:?}; fetches: {fetches}"
    );
    assert_eq!(
        sessions_after_entries, 0,
        "sessions while the payloads waited"
    );

    provider.shutdown().await?;
    joining.shutdown().await?;
    Ok(())
}

/// A gossip neighbor that lacks a payload the replica waits for still has
/// its own payload fetched at once.
///
/// The writer serves no payload, so the neighbor holds the first entry
/// without its payload. The neighbor writes only after a session it ran as
/// a neighbor has ended, since that end is what asks it for the payload it
/// lacks; a pause from that answer is at least 1 s, which sizes the bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_neighbor_lacking_a_payload_has_its_own_fetched_at_once() -> Result<()> {
    let mut rng = test_rng(b"a_neighbor_lacking_a_payload_has_its_own_fetched_at_once");
    let writer = loopback_node(&mut rng, |node| node.without_blob_serving()).await?;
    let neighbor = loopback_node(&mut rng, |node| node).await?;
    let replica = loopback_node(&mut rng, |node| node).await?;
    let doc = writer.docs().create().await?;
    let author = writer.docs().author_create().await?;
    doc.set_bytes(author, b"record/lacked".to_vec(), b"lacked".to_vec())
        .await?;
    let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
    let (doc_neighbor, mut neighbor_events) =
        util::import_and_subscribe(neighbor.docs(), ticket).await?;
    next_event_matching(&mut neighbor_events, TIMEOUT, |event| {
        matches!(event, LiveEvent::InsertRemote { .. })
    })
    .await;

    let ticket = doc_neighbor
        .share(AddrInfoOptions::RelayAndAddresses)
        .await?;
    let (_doc_replica, mut events) = util::import_and_subscribe(replica.docs(), ticket).await?;
    let neighbor_id = neighbor.id();
    next_event_matching(
        &mut events,
        TIMEOUT,
        |event| matches!(event, LiveEvent::NeighborUp(peer) if *peer == neighbor_id),
    )
    .await;
    next_event_matching(&mut events, TIMEOUT, |event| {
        match_sync_finished(event, neighbor_id)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let author = neighbor.docs().author_create().await?;
    let own = doc_neighbor
        .set_bytes(author, b"record/own".to_vec(), b"own".to_vec())
        .await?;
    next_event_matching(&mut events, TIMEOUT, |event| {
        matches!(event, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == own)
    })
    .await;
    let inserted = Instant::now();
    next_event_matching(
        &mut events,
        TIMEOUT,
        |event| matches!(event, LiveEvent::ContentReady { hash } if *hash == own),
    )
    .await;
    let fetches = replica.engine().fetches_report().await?;
    assert!(
        inserted.elapsed() < Duration::from_secs(1),
        "the neighbor's own payload took {:?}; fetches: {fetches}",
        inserted.elapsed()
    );

    writer.shutdown().await?;
    neighbor.shutdown().await?;
    replica.shutdown().await?;
    Ok(())
}

/// This tests basic sync and gossip with 3 peers.
#[tokio::test]
#[traced_test]
async fn sync_full_basic() -> testresult::TestResult<()> {
    let mut rng = test_rng(b"sync_full_basic");
    let mut nodes = spawn_nodes(2, &mut rng).await?;
    let mut clients = nodes
        .iter()
        .map(|node| node.client().clone())
        .collect::<Vec<_>>();

    // peer0: create doc and ticket
    let peer0 = nodes[0].id();
    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let blobs0 = clients[0].blobs();
    let mut events0 = doc0.subscribe().await?;
    let key0 = b"k1";
    let value0 = b"v1";
    let hash0 = doc0
        .set_bytes(author0, key0.to_vec(), value0.to_vec())
        .await?;

    info!("peer0: wait for 1 event (local insert)");
    let e = next(&mut events0).await;
    assert!(
        matches!(&e, LiveEvent::InsertLocal { entry } if entry.content_hash() == hash0),
        "expected LiveEvent::InsertLocal but got {e:?}",
    );
    assert_latest(blobs0, &doc0, key0, value0).await;
    let ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;

    info!("peer1: spawn");
    let peer1 = nodes[1].id();
    let author1 = clients[1].docs().author_create().await?;
    info!("peer1: join doc");
    // Subscribed before the sync starts, as in `sync_simple`.
    let (doc1, mut events1) = util::import_and_subscribe(clients[1].docs(), ticket.clone()).await?;
    let blobs1 = clients[1].blobs();

    info!("peer1: wait for 4 events (for sync and join with peer0)");
    assert_next_unordered(
        &mut events1,
        TIMEOUT,
        vec![
            match_event!(LiveEvent::NeighborUp(peer) if *peer == peer0),
            match_event!(LiveEvent::InsertRemote { from, .. } if *from == peer0 ),
            Box::new(move |e| match_sync_finished(e, peer0)),
            match_event!(LiveEvent::ContentReady { hash } if *hash == hash0),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;

    info!("peer0: wait for 2 events (join & accept sync finished from peer1)");
    assert_next(
        &mut events0,
        TIMEOUT,
        vec![
            match_event!(LiveEvent::NeighborUp(peer) if *peer == peer1),
            Box::new(move |e| match_sync_finished(e, peer1)),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;

    info!("peer1: insert entry");
    let key1 = b"k2";
    let value1 = b"v2";
    let hash1 = doc1
        .set_bytes(author1, key1.to_vec(), value1.to_vec())
        .await?;
    assert_latest(blobs1, &doc1, key1, value1).await;
    info!("peer1: wait for 1 event (local insert, and pendingcontentready)");
    assert_next(
        &mut events1,
        TIMEOUT,
        vec![match_event!(LiveEvent::InsertLocal { entry} if entry.content_hash() == hash1)],
    )
    .await;

    // peer0: assert events for the entry peer1 announced. The topic carries
    // peer1's author head, never the entry, so the entry arrives over the
    // reconciliation that announcement triggers — and that sync's own
    // `SyncFinished` and `PendingContentReady` fall between the two
    // checkpoints, which is why the surrounding events are ignored here.
    info!("peer0: wait for the entry announced by peer1");
    assert_events_matching(
        &mut events0,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == peer1)),
            Box::new(move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == hash1)),
        ],
    )
    .await;
    assert_latest(blobs0, &doc0, key1, value1).await;

    // Note: If we could check gossip messages directly here (we can't easily), we would notice
    // that peer1 will receive a `Op::ContentReady` gossip message, broadcast
    // by peer0 with neighbor scope. This message is superfluous, and peer0 could know that, however
    // our gossip implementation does not allow us to filter message receivers this way.

    info!("peer2: spawn");
    nodes.push(spawn_node(nodes.len(), &mut rng).await?);
    clients.push(nodes.last().unwrap().client().clone());
    // Subscribed before the sync starts, as in `sync_simple`.
    let (doc2, mut events2) = util::import_and_subscribe(clients[2].docs(), ticket).await?;
    let blobs2 = clients[2].blobs();
    let peer2 = nodes[2].id();

    // How many syncs peer2 runs is not fixed: it reconciles on the ticket's
    // peer, again on each `NeighborUp`, and once more for every author head
    // announced meanwhile, so the number of `SyncFinished` and
    // `PendingContentReady` events around the checkpoints varies. The
    // checkpoints themselves do not — both neighbors, a finished sync with
    // each, both entries, both contents.
    //
    // An entry's `content_status` is not one of them: it is a snapshot of the
    // local blob store taken as the event is produced, so what it reports and
    // what the store holds when the test reads the event need not agree.
    info!("peer2: wait for the events of its sync with both peers");
    assert_events_matching(
        &mut events2,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer0)),
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer1)),
            Box::new(move |e| match_sync_finished(e, peer0)),
            Box::new(move |e| match_sync_finished(e, peer1)),
            Box::new(
                move |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash0),
            ),
            Box::new(
                move |e| matches!(e, LiveEvent::InsertRemote { entry, .. } if entry.content_hash() == hash1),
            ),
            Box::new(move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == hash0)),
            Box::new(move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == hash1)),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;
    assert_latest(blobs2, &doc2, b"k1", b"v1").await;
    assert_latest(blobs2, &doc2, b"k2", b"v2").await;

    info!("peer0: wait for peer2 to join and its sync to finish");
    assert_events_matching(
        &mut events0,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer2)),
            Box::new(move |e| match_sync_finished(e, peer2)),
        ],
    )
    .await;

    info!("peer1: wait for peer2 to join and its sync to finish");
    assert_events_matching(
        &mut events1,
        TIMEOUT,
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(peer) if *peer == peer2)),
            Box::new(move |e| match_sync_finished(e, peer2)),
        ],
    )
    .await;

    info!("shutdown");
    for node in nodes {
        node.shutdown().await?;
    }

    Ok(())
}

/// `leave_gossip` leaves the swarm `start_sync` joined and touches nothing
/// else: a subscriber keeps receiving the replica's events, the replica
/// stays writable, and its live sync keeps serving a peer's dial. Leaving a
/// swarm that was never joined is a no-op, and a later `start_sync`
/// re-joins.
#[tokio::test]
#[traced_test]
async fn sync_leave_gossip_keeps_sync_running() -> Result<()> {
    let mut rng = test_rng(b"sync_leave_gossip_keeps_sync_running");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let (node, peer) = (&nodes[0], &nodes[1]);

    let doc = node.docs().create().await?;
    let author = node.docs().author_create().await?;

    doc.leave_gossip().await?;

    doc.start_sync(vec![], util::TEST_HOLDER).await?;
    let mut events = doc.subscribe().await?;
    doc.leave_gossip().await?;
    doc.leave_gossip().await?;
    doc.set_bytes(author, b"k".to_vec(), b"v".to_vec()).await?;
    next_event_matching(
        &mut events,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertLocal { entry } if entry.key() == b"k"),
    )
    .await;

    // Out of the swarm, the live sync still serves a peer that dials it.
    let ticket = doc.ticket(AddrInfoOptions::RelayAndAddresses).await?;
    let theirs = peer
        .docs()
        .import_namespace(ticket.capability.clone())
        .await?;
    theirs
        .sync_with_peers(ticket.contacts(), vec![], util::TEST_HOLDER, false)
        .await?;
    let deadline = Instant::now() + TIMEOUT;
    while get_exact(&theirs, author, b"k", false).await?.is_none() {
        assert!(
            Instant::now() < deadline,
            "a replica out of its swarm served no dial"
        );
        n0_future::time::sleep(Duration::from_millis(100)).await;
    }

    doc.start_sync(vec![], util::TEST_HOLDER).await?;

    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// A replica whose ticket names its peer by relay alone syncs through the
/// relay: it takes the peer's entry with its content, and the update after
/// it.
#[tokio::test]
#[traced_test]
async fn test_sync_via_relay() -> Result<()> {
    let mut rng = test_rng(b"test_sync_via_relay");
    let (relay_map, _relay_url, _guard) = iroh::test_utils::run_relay_server().await?;

    use crate::util::endpoint;

    let ep1 = endpoint(
        SecretKey::from_bytes(&rng.random()),
        relay_map.clone(),
        None,
    )
    .await?;
    let node1 = Node::memory(ep1).spawn().await?;
    let node1_id = node1.id();
    let ep2 = endpoint(
        SecretKey::from_bytes(&rng.random()),
        relay_map.clone(),
        None,
    )
    .await?;
    let node2 = Node::memory(ep2).spawn().await?;

    node1.online().await;
    node2.online().await;
    let doc1 = node1.docs().create().await?;
    let author1 = node1.docs().author_create().await?;
    let inserted_hash = doc1
        .set_bytes(author1, b"foo".to_vec(), b"bar".to_vec())
        .await?;
    let mut ticket = doc1.share(AddrInfoOptions::RelayAndAddresses).await?;

    // remove direct addrs to force connect via relay
    let mut relay_ticket = ticket.nodes[0].clone();
    relay_ticket.addrs = relay_ticket
        .addrs
        .iter()
        .filter(|addr| matches!(addr, iroh::TransportAddr::Relay(_)))
        .cloned()
        .collect();
    ticket.nodes[0] = relay_ticket;
    // join
    // Subscribed before the sync starts, as in `sync_simple`.
    let (doc2, mut events) = util::import_and_subscribe(node2.docs(), ticket).await?;
    let blobs2 = node2.blobs();

    // An `InsertRemote`'s `content_status` is a snapshot of the local blob
    // store taken as the event is produced, and `ContentReady` reaches this
    // stream over a channel of its own that carries no order against it — so
    // the status is not asserted here. `ContentReady` is what proves the
    // content arrived.
    assert_next_unordered_with_optionals(
        &mut events,
        Duration::from_secs(2),
        vec![
            Box::new(move |e| matches!(e, LiveEvent::NeighborUp(n) if *n== node1_id)),
            Box::new(move |e| match_sync_finished(e, node1_id)),
            Box::new(
                move |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == node1_id),
            ),
            Box::new(
                move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == inserted_hash),
            ),
            match_event!(LiveEvent::PendingContentReady),
        ],
        vec![Box::new(move |e| match_sync_finished(e, node1_id))],
    )
    .await;
    let actual = blobs2
        .get_bytes(
            get_exact(&doc2, author1, b"foo", false)
                .await?
                .expect("entry to exist")
                .content_hash(),
        )
        .await?;
    assert_eq!(actual.as_ref(), b"bar");

    // update
    let updated_hash = doc1
        .set_bytes(author1, b"foo".to_vec(), b"update".to_vec())
        .await?;
    assert_next_unordered_with_optionals(
        &mut events,
        Duration::from_secs(10),
        vec![
            Box::new(
                move |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == node1_id),
            ),
            Box::new(
                move |e| matches!(e, LiveEvent::ContentReady { hash } if *hash == updated_hash),
            ),
        ],
        vec![
            Box::new(move |e| match_sync_finished(e, node1_id)),
            Box::new(move |e| matches!(e, LiveEvent::PendingContentReady)),
        ],
    )
    .await;
    let actual = blobs2
        .get_bytes(
            get_exact(&doc2, author1, b"foo", false)
                .await?
                .expect("entry to exist")
                .content_hash(),
        )
        .await?;
    assert_eq!(actual.as_ref(), b"update");
    Ok(())
}

/// A node restarted on its storage directory reopens its replica with what
/// it held, takes at its first sync a write its peer made while it was down,
/// and takes the peer's later writes live.
#[tokio::test]
#[traced_test]
async fn sync_restart_node() -> Result<()> {
    use crate::util::endpoint;

    let mut rng = test_rng(b"sync_restart_node");
    let (relay_map, _relay_url, _guard) = iroh::test_utils::run_relay_server().await?;

    let lookup_server = iroh::test_utils::DnsPkarrServer::run().await?;

    let node1_dir = tempfile::TempDir::with_prefix("test-sync_restart_node-node1")?;
    let secret_key_1 = SecretKey::from_bytes(&rng.random());

    let ep = endpoint(
        secret_key_1.clone(),
        relay_map.clone(),
        Some(&lookup_server),
    )
    .await?;
    let node1 = Node::persistent(&node1_dir, ep).spawn().await?;
    let id1 = node1.id();

    // create doc & ticket on node1
    let doc1 = node1.docs().create().await?;
    let blobs1 = node1.blobs();
    let mut events1 = doc1.subscribe().await?;
    let ticket = doc1.share(AddrInfoOptions::RelayAndAddresses).await?;

    // create node2
    let secret_key_2 = SecretKey::from_bytes(&rng.random());
    let ep = endpoint(secret_key_2, relay_map.clone(), Some(&lookup_server)).await?;
    let node2 = Node::memory(ep).spawn().await?;
    let id2 = node2.id();
    let author2 = node2.docs().author_create().await?;
    let doc2 = util::import(node2.docs(), ticket.clone()).await?;
    let blobs2 = node2.blobs();

    info!("node2 set a");
    let hash_a = doc2.set_bytes(author2, "n2/a", "a").await?;
    assert_latest(blobs2, &doc2, b"n2/a", b"a").await;

    assert_next_unordered_with_optionals(
        &mut events1,
        Duration::from_secs(10),
        vec![
            match_event!(LiveEvent::NeighborUp(n) if *n == id2),
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::InsertRemote { from, content_status: ContentStatus::Missing, .. } if *from == id2),
            match_event!(LiveEvent::ContentReady { hash } if *hash == hash_a),
            match_event!(LiveEvent::PendingContentReady),
        ],
        vec![
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::PendingContentReady),
        ],
    )
    .await;
    assert_latest(blobs1, &doc1, b"n2/a", b"a").await;

    info!(me = %id1.fmt_short(), "node1 start shutdown");
    node1.shutdown().await?;
    info!(me = %id1.fmt_short(), "node1 down");

    info!(me = %id1.fmt_short(), "sleep 1s");
    n0_future::time::sleep(Duration::from_secs(1)).await;

    info!(me = %id2.fmt_short(), "node2 set b");
    let hash_b = doc2.set_bytes(author2, "n2/b", "b").await?;

    info!(me = %id1.fmt_short(), "node1 respawn");
    let ep = endpoint(
        secret_key_1.clone(),
        relay_map.clone(),
        Some(&lookup_server),
    )
    .await?;
    let node1 = Node::persistent(&node1_dir, ep).spawn().await?;
    assert_eq!(id1, node1.id());

    let doc1 = node1.docs().open(doc1.id()).await?.expect("doc to exist");
    let blobs1 = node1.blobs();
    let mut events1 = doc1.subscribe().await?;
    assert_latest(blobs1, &doc1, b"n2/a", b"a").await;

    // check that initial resync is working
    doc1.start_sync(vec![], util::TEST_HOLDER).await?;
    assert_next_unordered_with_optionals(
        &mut events1,
        Duration::from_secs(10),
        vec![
            match_event!(LiveEvent::NeighborUp(n) if *n== id2),
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::InsertRemote { from, content_status: ContentStatus::Missing, .. } if *from == id2),
            match_event!(LiveEvent::ContentReady { hash } if *hash == hash_b),
        ],
        vec![
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::PendingContentReady),
        ]
    ).await;
    assert_latest(blobs1, &doc1, b"n2/b", b"b").await;

    // check that live conn is working
    info!(me = %id2.fmt_short(), "node2 set c");
    let hash_c = doc2.set_bytes(author2, "n2/c", "c").await?;
    assert_next_unordered_with_optionals(
        &mut events1,
        Duration::from_secs(10),
        vec![
            match_event!(LiveEvent::InsertRemote { from, content_status: ContentStatus::Missing, .. } if *from == id2),
            match_event!(LiveEvent::ContentReady { hash } if *hash == hash_c),
        ],
        vec![
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::PendingContentReady),
            match_event!(LiveEvent::SyncFinished(e) if e.peer == id2 && e.result.is_ok()),
            match_event!(LiveEvent::PendingContentReady),
        ]
    ).await;

    assert_latest(blobs1, &doc1, b"n2/c", b"c").await;

    Ok(())
}

/// Test sync between many nodes with propagation through sync reports.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn sync_big() -> Result<()> {
    let mut rng = test_rng(b"sync_big");
    let n_nodes = std::env::var("NODES")
        .map(|v| v.parse().expect("NODES must be a number"))
        .unwrap_or(10);
    let n_entries_init = 1;

    tokio::task::spawn(async move {
        let mut i = 0u64;
        loop {
            n0_future::time::sleep(Duration::from_secs(1)).await;
            info!("tick {i}");
            i += 1;
        }
    });

    let nodes = spawn_nodes(n_nodes, &mut rng).await?;
    let node_ids = nodes.iter().map(|node| node.id()).collect::<Vec<_>>();
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();
    let authors = collect_futures(clients.iter().map(|c| c.docs().author_create())).await?;

    let doc0 = clients[0].docs().create().await?;
    let mut ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;
    // Imported without peer info: each node's own data is asserted before
    // any node joins.
    let peer0 = ticket.nodes[0].clone();
    ticket.nodes = vec![];

    let docs_clients: Vec<_> = clients.iter().skip(1).collect();
    let mut docs = vec![];
    docs.push(doc0);
    docs.extend_from_slice(
        &collect_futures(docs_clients.into_iter().map(|c| {
            let ticket = ticket.clone();
            async move { util::import(c.docs(), ticket).await }
        }))
        .await?,
    );

    let mut expected = vec![];

    // create initial data on each node
    publish(&docs, &mut expected, n_entries_init, |i, j| {
        (
            authors[i],
            format!("init/{}/{j}", node_ids[i].fmt_short()),
            format!("init:{i}:{j}"),
        )
    })
    .await?;

    // assert initial data
    for (i, doc) in docs.iter().enumerate() {
        let blobs = nodes[i].blobs();
        let entries = get_all_with_content(blobs, doc).await?;
        let mut expected = expected
            .iter()
            .filter(|e| e.author == authors[i])
            .cloned()
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(entries, expected, "phase1 pre-sync correct");
    }

    // setup event streams
    let events = collect_futures(docs.iter().map(|d| d.subscribe())).await?;

    // join nodes together
    for (i, doc) in docs.iter().enumerate().skip(1) {
        info!(me = %node_ids[i].fmt_short(), peer = %peer0.id.fmt_short(), "join");
        doc.start_sync(util::contacts([peer0.clone()]), util::TEST_HOLDER)
            .await?;
    }

    // wait for InsertRemote events stuff to happen
    info!("wait for all peers to receive insert events");
    let expected_inserts = (n_nodes - 1) * n_entries_init;
    let mut tasks = tokio::task::JoinSet::default();
    for (i, events) in events.into_iter().enumerate() {
        let doc = docs[i].clone();
        let me = doc.id().fmt_short();
        let expected = expected.clone();
        let fut = async move {
            wait_for_events(events, expected_inserts, TIMEOUT, |e| {
                matches!(e, LiveEvent::InsertRemote { .. })
            })
            .await?;
            let entries = get_all(&doc).await?;
            if entries != expected {
                Err(anyhow!(
                    "node {i} failed (has {} entries but expected to have {})",
                    entries.len(),
                    expected.len()
                ))
            } else {
                info!(
                    "received and checked all {} expected entries",
                    expected.len()
                );
                Ok(())
            }
        }
        .instrument(error_span!("sync-test", %me));
        let fut = async move {
            let r = fut.await;
            r.with_context(move || format!("node {i} ({me})"))
        };
        tasks.spawn(fut);
    }

    while let Some(res) = tasks.join_next().await {
        res??;
    }

    assert_all_docs(&docs, &node_ids, &expected, "after initial sync").await;

    info!("shutdown");
    for node in nodes {
        node.shutdown().await?;
    }

    Ok(())
}

/// Opening every replica a listing names while the listing is still being
/// read finishes — 200 replicas within 2 s — rather than deadlocking the
/// store's actor.
#[tokio::test]
#[traced_test]
async fn test_list_docs_stream() -> testresult::TestResult<()> {
    let node = Node::memory(empty_endpoint().await?).spawn().await?;
    let count = 200;

    // create docs
    for _i in 0..count {
        node.docs().create().await?;
    }

    // create doc stream
    let mut stream = node.docs().list().await?;

    // process each doc and call into the docs actor.
    // this makes sure that we don't deadlock the docs actor.
    let mut i = 0;
    let fut = async {
        while let Some((id, _)) = stream.try_next().await.unwrap() {
            let _doc = node.docs().open(id).await.unwrap().unwrap();
            i += 1;
        }
    };

    n0_future::time::timeout(Duration::from_secs(2), fut)
        .await
        .expect("not to timeout");

    assert_eq!(i, count);

    Ok(())
}

/// `author`'s entry at `key`, read as the data layer reads one entry.
async fn get_exact(
    doc: &Doc,
    author: AuthorId,
    key: impl AsRef<[u8]>,
    include_empty: bool,
) -> anyhow::Result<Option<Entry>> {
    let query = Query::author(author).key_exact(key);
    if include_empty {
        doc.get_one(query.include_empty()).await
    } else {
        doc.get_one(query).await
    }
}

/// Get all entries of a document.
async fn get_all(doc: &Doc) -> anyhow::Result<Vec<Entry>> {
    let entries = doc.get_many(Query::all()).await?;
    entries.try_collect::<Vec<_>>().await
}

/// Get all entries of a document with the blob content.
async fn get_all_with_content(
    blobs: &iroh_blobs::api::Store,
    doc: &Doc,
) -> anyhow::Result<Vec<(Entry, Bytes)>> {
    let entries = doc.get_many(Query::all()).await?;
    let entries = entries.and_then(|entry| async {
        let hash = entry.content_hash();
        let content = blobs.get_bytes(hash).await.map_err(anyhow::Error::from);
        content.map(|c| (entry, c))
    });
    entries.try_collect::<Vec<_>>().await
}

async fn publish(
    docs: &[Doc],
    expected: &mut Vec<ExpectedEntry>,
    n: usize,
    cb: impl Fn(usize, usize) -> (AuthorId, String, String),
) -> anyhow::Result<()> {
    for (i, doc) in docs.iter().enumerate() {
        for j in 0..n {
            let (author, key, value) = cb(i, j);
            doc.set_bytes(author, key.as_bytes().to_vec(), value.as_bytes().to_vec())
                .await?;
            expected.push(ExpectedEntry { author, key, value });
        }
    }
    expected.sort();
    Ok(())
}

/// Collect an iterator into futures by joining them all and failing if any future failed.
async fn collect_futures<T>(
    futs: impl IntoIterator<Item = impl Future<Output = anyhow::Result<T>>>,
) -> anyhow::Result<Vec<T>> {
    futures_buffered::join_all(futs)
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
}

/// Collect `count` events from the `events` stream, only collecting events for which `matcher`
/// returns true.
async fn wait_for_events(
    mut events: impl Stream<Item = Result<LiveEvent>> + Send + Unpin + 'static,
    count: usize,
    timeout: Duration,
    matcher: impl Fn(&LiveEvent) -> bool,
) -> anyhow::Result<Vec<LiveEvent>> {
    let mut res = Vec::with_capacity(count);
    let sleep = n0_future::time::sleep(timeout);
    tokio::pin!(sleep);
    while res.len() < count {
        tokio::select! {
            () = &mut sleep => {
                bail!("Failed to collect {count} elements in {timeout:?} (collected only {})", res.len());
            },
            event = events.try_next() => {
                let event = event?;
                match event {
                    None => bail!("stream ended after {} items, but expected {count}", res.len()),
                    Some(event) => if matcher(&event) {
                        res.push(event);
                        debug!("recv event {} of {count}", res.len());
                    }
                }
            }
        }
    }
    Ok(res)
}

async fn assert_all_docs(
    docs: &[Doc],
    node_ids: &[PublicKey],
    expected: &Vec<ExpectedEntry>,
    label: &str,
) {
    info!("validate all peers: {label}");
    for (i, doc) in docs.iter().enumerate() {
        let entries = get_all(doc).await.unwrap_or_else(|err| {
            panic!("failed to get entries for peer {:?}: {err:?}", node_ids[i])
        });
        assert_eq!(
            &entries,
            expected,
            "{label}: peer {i} {:?} failed (have {} but expected {})",
            node_ids[i],
            entries.len(),
            expected.len()
        );
    }
}

#[derive(Debug, Ord, Eq, PartialEq, PartialOrd, Clone)]
struct ExpectedEntry {
    author: AuthorId,
    key: String,
    value: String,
}

impl PartialEq<Entry> for ExpectedEntry {
    fn eq(&self, other: &Entry) -> bool {
        self.key.as_bytes() == other.key()
            && Hash::new(&self.value) == other.content_hash()
            && self.author == other.author()
    }
}
impl PartialEq<(Entry, Bytes)> for ExpectedEntry {
    fn eq(&self, (entry, content): &(Entry, Bytes)) -> bool {
        self.key.as_bytes() == entry.key()
            && Hash::new(&self.value) == entry.content_hash()
            && self.author == entry.author()
            && self.value.as_bytes() == content.as_ref()
    }
}
impl PartialEq<ExpectedEntry> for Entry {
    fn eq(&self, other: &ExpectedEntry) -> bool {
        other.eq(self)
    }
}
impl PartialEq<ExpectedEntry> for (Entry, Bytes) {
    fn eq(&self, other: &ExpectedEntry) -> bool {
        other.eq(self)
    }
}

/// A deleted entry reads as absent, and its payload leaves the blob store at
/// the next blob collection.
#[tokio::test]
#[traced_test]
async fn doc_delete() -> Result<()> {
    let tempdir = tempdir()?;
    // A file-backed node: the suites' in-memory one runs no blob collection.
    let ep = empty_endpoint().await?;
    let node = Node::persistent(tempdir.path(), ep)
        .gc_interval(Some(Duration::from_millis(100)))
        .spawn()
        .await?;
    let client = node.client();
    let doc = client.docs().create().await?;
    let blobs = client.blobs();
    let author = client.docs().author_create().await?;
    let hash = doc
        .set_bytes(author, b"foo".to_vec(), b"hi".to_vec())
        .await?;
    assert_latest(blobs, &doc, b"foo", b"hi").await;
    let deleted = doc.del(author, b"foo".to_vec()).await?;
    assert_eq!(deleted, 1);

    let entry = get_exact(&doc, author, b"foo".to_vec(), false).await?;
    assert!(entry.is_none());

    // Twenty intervals of the collection, which runs on its own schedule.
    n0_future::time::sleep(Duration::from_secs(2)).await;
    let bytes = client.blobs().get_bytes(hash).await;
    assert!(bytes.is_err());
    node.shutdown().await?;
    Ok(())
}

/// A dropped replica fails every call of its handle, opens as absent, and
/// ends its subscription.
#[tokio::test]
#[traced_test]
async fn sync_drop_doc() -> Result<()> {
    let mut rng = test_rng(b"sync_drop_doc");
    let node = spawn_node(0, &mut rng).await?;
    let client = node.client();

    let doc = client.docs().create().await?;
    let author = client.docs().author_create().await?;

    let mut sub = doc.subscribe().await?;
    doc.set_bytes(author, b"foo".to_vec(), b"bar".to_vec())
        .await?;
    let ev = sub.try_next().await;
    assert!(matches!(ev, Ok(Some(LiveEvent::InsertLocal { .. }))));

    client.docs().drop_doc(doc.id()).await?;
    let res = get_exact(&doc, author, b"foo".to_vec(), true).await;
    assert!(res.is_err());
    let res = doc
        .set_bytes(author, b"foo".to_vec(), b"bar".to_vec())
        .await;
    assert!(res.is_err());
    assert!(client.docs().open(doc.id()).await?.is_none());
    let ev = sub.try_next().await?;
    assert!(ev.is_none());

    Ok(())
}

/// A replica dropped while a start of its sync waits behind the drop is
/// deleted, and the drop succeeds: the start finds no replica rather than
/// reopening it between the leave and the delete. Repeated, since a start
/// lands in that gap only some of the time.
#[tokio::test]
#[traced_test]
async fn a_drop_with_a_start_of_sync_behind_it_deletes_the_replica() -> Result<()> {
    let mut rng = test_rng(b"a_drop_with_a_start_of_sync_behind_it_deletes_the_replica");
    let node = spawn_node(0, &mut rng).await?;
    let docs = node.client().docs();
    for _ in 0..20 {
        let doc = docs.create().await?;
        doc.start_sync(vec![], util::TEST_HOLDER).await?;
        let (dropped, _started) = tokio::join!(
            docs.drop_doc(doc.id()),
            doc.start_sync(vec![], util::TEST_HOLDER)
        );
        dropped?;
        let mut listed = docs.list().await?;
        while let Some((id, _kind)) = listed.try_next().await? {
            assert_ne!(id, doc.id(), "the dropped replica is still held");
        }
    }
    node.shutdown().await?;
    Ok(())
}

/// A replica dropped while another open of it is outstanding is deleted,
/// and the drop succeeds. The handle that open gave fails its next call, as
/// every handle of a dropped replica does.
#[tokio::test]
#[traced_test]
async fn a_drop_with_another_open_outstanding_deletes_the_replica() -> Result<()> {
    let mut rng = test_rng(b"a_drop_with_another_open_outstanding_deletes_the_replica");
    let node = spawn_node(0, &mut rng).await?;
    let docs = node.client().docs();
    let doc = docs.create().await?;
    doc.start_sync(vec![], util::TEST_HOLDER).await?;
    let author = docs.author_create().await?;
    let other = docs
        .open(doc.id())
        .await?
        .ok_or_else(|| anyhow!("the created replica does not open"))?;

    docs.drop_doc(doc.id()).await?;
    let mut listed = docs.list().await?;
    while let Some((id, _kind)) = listed.try_next().await? {
        assert_ne!(id, doc.id(), "the dropped replica is still held");
    }
    assert!(
        other
            .set_bytes(author, b"key".to_vec(), b"value".to_vec())
            .await
            .is_err(),
        "a handle of the dropped replica still writes"
    );
    node.shutdown().await?;
    Ok(())
}

async fn assert_latest(blobs: &iroh_blobs::api::Store, doc: &Doc, key: &[u8], value: &[u8]) {
    let content = get_latest(blobs, doc, key).await.unwrap();
    assert_eq!(content, value.to_vec());
}

async fn get_latest(
    blobs: &iroh_blobs::api::Store,
    doc: &Doc,
    key: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let query = Query::single_latest_per_key().key_exact(key);
    let stream = doc.get_many(query).await?;
    tokio::pin!(stream);
    let entry = stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow!("entry not found"))?;
    let content = blobs.get_bytes(entry.content_hash()).await?;
    Ok(content.to_vec())
}

async fn next<T: std::fmt::Debug>(mut stream: impl Stream<Item = Result<T>> + Unpin) -> T {
    let event = stream
        .try_next()
        .await
        .expect("stream ended")
        .expect("stream produced error");
    debug!("Event: {event:?}");
    event
}

#[allow(clippy::type_complexity)]
fn apply_matchers<T>(item: &T, matchers: &mut Vec<Box<dyn Fn(&T) -> bool + Send>>) -> bool {
    for i in 0..matchers.len() {
        if matchers[i](item) {
            let _ = matchers.remove(i);
            return true;
        }
    }
    false
}

/// The bookkeeping of an exchange no scenario asserts: a sync trigger that
/// meets a running exchange is run again after it, so a join can bring one
/// more successful `SyncFinished`, and its `PendingContentReady`, than a
/// scenario counts. The helpers below skip such an event when no matcher
/// takes it.
fn extra_exchange(event: &LiveEvent) -> bool {
    matches!(event, LiveEvent::SyncFinished(sync) if sync.result.is_ok())
        || matches!(event, LiveEvent::PendingContentReady)
}

/// Receive the next `matchers.len()` elements from a stream and matches them against the functions
/// in `matchers`, in order.
///
/// Returns all received events.
#[allow(clippy::type_complexity)]
async fn assert_next(
    mut stream: impl Stream<Item = Result<LiveEvent>> + Unpin + Send,
    timeout: Duration,
    matchers: Vec<Box<dyn Fn(&LiveEvent) -> bool + Send>>,
) -> Vec<LiveEvent> {
    let fut = async {
        let mut items = vec![];
        for (i, f) in matchers.iter().enumerate() {
            let item = loop {
                let item = stream
                    .try_next()
                    .await
                    .expect("event stream ended prematurely")
                    .expect("event stream errored");
                if (f)(&item) || !extra_exchange(&item) {
                    break item;
                }
            };
            if !(f)(&item) {
                panic!("assertion failed for event {i} {item:?}");
            }
            items.push(item);
        }
        items
    };
    let res = n0_future::time::timeout(timeout, fut).await;
    res.expect("timeout reached")
}

/// Receive `matchers.len()` elements from a stream and assert that each element matches one of the
/// functions in `matchers`.
///
/// Order of the matchers is not relevant.
///
/// Returns all received events.
#[allow(clippy::type_complexity)]
async fn assert_next_unordered(
    stream: impl Stream<Item = Result<LiveEvent>> + Unpin + Send,
    timeout: Duration,
    matchers: Vec<Box<dyn Fn(&LiveEvent) -> bool + Send>>,
) -> Vec<LiveEvent> {
    assert_next_unordered_with_optionals(stream, timeout, matchers, vec![]).await
}

/// Receive between `min` and `max` elements from the stream and assert that each element matches
/// either one of the matchers in `required_matchers` or in `optional_matchers`.
///
/// Order of the matchers is not relevant.
///
/// Will return an error if:
/// * Any element fails to match one of the required or optional matchers
/// * More than `max` elements were received, but not all required matchers were used yet
/// * The timeout completes before all required matchers were used
///
/// Returns all received events.
#[allow(clippy::type_complexity)]
async fn assert_next_unordered_with_optionals(
    mut stream: impl Stream<Item = Result<LiveEvent>> + Unpin + Send,
    timeout: Duration,
    mut required_matchers: Vec<Box<dyn Fn(&LiveEvent) -> bool + Send>>,
    mut optional_matchers: Vec<Box<dyn Fn(&LiveEvent) -> bool + Send>>,
) -> Vec<LiveEvent> {
    let max = required_matchers.len() + optional_matchers.len();
    let required = required_matchers.len();
    // we have to use a mutex because rustc is not intelligent enough to realize
    // that the mutable borrow terminates when the future completes
    let events = Arc::new(parking_lot::Mutex::new(vec![]));
    let fut = async {
        let mut matched = 0;
        while let Ok(event) = stream.try_next().await {
            let event = event.context("failed to read from stream")?;
            events.lock().push(event.clone());
            if !apply_matchers(&event, &mut required_matchers)
                && !apply_matchers(&event, &mut optional_matchers)
            {
                if extra_exchange(&event) {
                    continue;
                }
                bail!("Event didn't match any matcher: {event:?}");
            }
            matched += 1;
            if required_matchers.is_empty() || matched == max {
                break;
            }
        }
        if !required_matchers.is_empty() {
            bail!(
                "Matched only {} of {required} required matchers",
                required - required_matchers.len()
            );
        }
        Ok(())
    };
    tokio::pin!(fut);
    let res = n0_future::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow!("Timeout reached ({timeout:?})"))
        .and_then(|res| res);
    let events = events.lock().clone();
    if let Err(err) = &res {
        println!("Received events: {events:#?}");
        println!(
            "Received {} events, expected between {required} and {max}",
            events.len()
        );
        panic!("Failed to receive or match all events: {err:?}");
    }
    events
}

/// Asserts that the event is a [`LiveEvent::SyncFinished`] and that the contained [`SyncEvent`]
/// has no error and matches `peer` and `namespace`.
fn match_sync_finished(event: &LiveEvent, peer: PublicKey) -> bool {
    let LiveEvent::SyncFinished(e) = event else {
        return false;
    };
    e.peer == peer && e.result.is_ok()
}

/// Receive events until every matcher has matched one, discarding events that
/// match none. Panics when the timeout elapses first.
///
/// The set form of [`next_event_matching`], for the same reason: a local
/// insert announces an author head to the neighbors and every neighbor pulls,
/// so how many sync and readiness events surround a checkpoint is not fixed.
#[allow(clippy::type_complexity)]
async fn assert_events_matching(
    stream: &mut (impl Stream<Item = Result<LiveEvent>> + Unpin + Send),
    timeout: Duration,
    mut matchers: Vec<Box<dyn Fn(&LiveEvent) -> bool + Send>>,
) -> Vec<LiveEvent> {
    let events = Arc::new(parking_lot::Mutex::new(vec![]));
    let fut = async {
        while !matchers.is_empty() {
            let event = stream
                .try_next()
                .await
                .expect("event stream errored")
                .expect("event stream ended");
            events.lock().push(event.clone());
            let _ = apply_matchers(&event, &mut matchers);
        }
    };
    let res = n0_future::time::timeout(timeout, fut).await;
    let events = events.lock().clone();
    if res.is_err() {
        println!("Received events: {events:#?}");
        panic!("Timeout reached ({timeout:?}) with unmatched matchers left");
    }
    events
}

/// Receive events from the stream until one matches, discarding the rest.
/// Panics when the timeout elapses first. For scenarios where gossip emits
/// a nondeterministic set of surrounding events (neighbor changes, repeated
/// readiness notifications) and only one specific event is the checkpoint.
async fn next_event_matching(
    stream: &mut (impl Stream<Item = Result<LiveEvent>> + Unpin + Send),
    timeout: Duration,
    matcher: impl Fn(&LiveEvent) -> bool,
) -> LiveEvent {
    let fut = async {
        loop {
            let event = stream
                .try_next()
                .await
                .expect("event stream errored")
                .expect("event stream ended");
            if matcher(&event) {
                break event;
            }
        }
    };
    n0_future::time::timeout(timeout, fut)
        .await
        .expect("timeout waiting for matching event")
}

/// A dial of a namespace ordered after another starts only once an
/// exchange of the other with the same peer has finished, and a dial the
/// order makes of the other runs though nothing asked for it.
#[tokio::test]
#[traced_test]
async fn a_dial_ordered_after_another_namespace_follows_its_exchange() -> Result<()> {
    let mut rng = test_rng(b"a_dial_ordered_after_another_namespace_follows_its_exchange");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let id1 = nodes[1].id();
    let (first0, then0) = (
        nodes[0].docs().create().await?,
        nodes[0].docs().create().await?,
    );
    let mut first_events = first0.subscribe().await?;
    let mut then_events = then0.subscribe().await?;
    let mut contacts = Vec::new();
    let mut imported = Vec::new();
    for doc in [&first0, &then0] {
        doc.sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
            .await?;
        let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
        contacts = ticket.contacts();
        imported.push(nodes[1].docs().import_namespace(ticket.capability).await?);
    }
    let (first1, then1) = (&imported[0], &imported[1]);
    nodes[1]
        .engine()
        .order_after(then1.id(), Some(first1.id()))
        .await?;
    // In the sync set, with no peer of its own to dial.
    first1
        .sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
        .await?;
    then1
        .sync_with_peers(contacts, vec![], util::TEST_HOLDER, false)
        .await?;

    let first = next_event_matching(&mut first_events, TIMEOUT, move |e| {
        match_sync_finished(e, id1)
    })
    .await;
    let then = next_event_matching(&mut then_events, TIMEOUT, move |e| {
        match_sync_finished(e, id1)
    })
    .await;
    let (LiveEvent::SyncFinished(first), LiveEvent::SyncFinished(then)) = (first, then) else {
        unreachable!("matched as finished syncs");
    };
    assert!(
        then.started >= first.finished,
        "the ordered dial started before the exchange it follows finished"
    );
    Ok(())
}

/// Node `x`, the greater id, with `then` ordered after `first`, and node
/// `y`. `x`'s dial of `first` waits in its provider until `y`'s dial of
/// `first` has been accepted and reached `x`'s entries, then takes
/// `answer`; `y` reveals its entries of `first` slowly, so the accepted
/// exchange outlasts whatever becomes of `x`'s dial by seconds. Answers the
/// accepted exchange of `first` and the exchange of `then` that `x` dialed.
async fn crossed_dials_of_first(
    seed: &[u8],
    answer: fn() -> SessionAccess,
) -> Result<(SyncEvent, SyncEvent)> {
    let mut rng = test_rng(seed);
    let (mut key_x, mut key_y) = (
        SecretKey::from_bytes(&rng.random()),
        SecretKey::from_bytes(&rng.random()),
    );
    if key_x.public() < key_y.public() {
        std::mem::swap(&mut key_x, &mut key_y);
    }
    let first_id: Arc<std::sync::OnceLock<NamespaceId>> = Arc::default();
    let dial_asked = Arc::new(Notify::new());
    let accepted = Arc::new(Notify::new());
    let gate = Arc::new(AtomicBool::new(true));
    let x_access: SessionAccessProvider = {
        let (first_id, dial_asked, accepted) =
            (first_id.clone(), dial_asked.clone(), accepted.clone());
        Arc::new(move |namespace, _identity, _caller, _peer, role| {
            let ours = first_id.get() == Some(&namespace);
            let (dial_asked, accepted, gate) = (dial_asked.clone(), accepted.clone(), gate.clone());
            Box::pin(async move {
                match role {
                    SessionRole::Dial if ours && gate.swap(false, Ordering::SeqCst) => {
                        dial_asked.notify_one();
                        accepted.notified().await;
                        answer()
                    }
                    SessionRole::Accept if ours => {
                        let reached: EntryFilter = Arc::new(move |_entry| {
                            accepted.notify_one();
                            true
                        });
                        SessionAccess::Allow {
                            egress: Some(reached),
                            ingest: pdn_store::admit_all(),
                            rests_on: RestsOn::Nothing,
                        }
                    }
                    _ => SessionAccess::whole(),
                }
            })
        })
    };
    let y_access: SessionAccessProvider = {
        let first_id = first_id.clone();
        Arc::new(move |namespace, _identity, _caller, _peer, role| {
            let slow = first_id.get() == Some(&namespace) && role == SessionRole::Dial;
            let access = if slow {
                let slow_egress: EntryFilter = Arc::new(|_entry| {
                    std::thread::sleep(Duration::from_millis(50));
                    true
                });
                SessionAccess::Allow {
                    egress: Some(slow_egress),
                    ingest: pdn_store::admit_all(),
                    rests_on: RestsOn::Nothing,
                }
            } else {
                SessionAccess::whole()
            };
            Box::pin(std::future::ready(access))
        })
    };
    let x = test_node(key_x).await?.access(x_access).spawn().await?;
    let y = test_node(key_y).await?.access(y_access).spawn().await?;
    let id_y = y.id();

    let (first_x, then_x) = (x.docs().create().await?, x.docs().create().await?);
    first_id
        .set(first_x.id())
        .map_err(|_| anyhow!("first set once"))?;
    let author_x = x.docs().author_create().await?;
    first_x
        .set_bytes(author_x, b"x".to_vec(), b"x".to_vec())
        .await?;
    let mut x_contacts = Vec::new();
    let mut imported = Vec::new();
    for doc in [&first_x, &then_x] {
        let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
        x_contacts = ticket.contacts();
        imported.push(y.docs().import_namespace(ticket.capability).await?);
    }
    let (first_y, then_y) = (&imported[0], &imported[1]);
    let author_y = y.docs().author_create().await?;
    for i in 0..20u8 {
        first_y.set_bytes(author_y, vec![b'y', i], vec![i]).await?;
    }
    let y_contacts = then_y
        .share(AddrInfoOptions::RelayAndAddresses)
        .await?
        .contacts();
    let mut first_events = first_x.subscribe().await?;
    let mut then_events = then_x.subscribe().await?;

    x.engine()
        .order_after(then_x.id(), Some(first_x.id()))
        .await?;
    first_x
        .sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
        .await?;
    then_x
        .sync_with_peers(y_contacts, vec![], util::TEST_HOLDER, false)
        .await?;
    n0_future::time::timeout(TIMEOUT, dial_asked.notified()).await?;
    then_y
        .sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
        .await?;
    first_y
        .sync_with_peers(x_contacts, vec![], util::TEST_HOLDER, false)
        .await?;

    let first = next_event_matching(&mut first_events, TIMEOUT, move |e| {
        match_sync_finished(e, id_y)
    })
    .await;
    // The counterpart dials `then` of its own accord once a neighbor; the
    // order holds this node's dials alone.
    let then = next_event_matching(&mut then_events, TIMEOUT, move |e| {
        match_sync_finished(e, id_y)
            && matches!(e, LiveEvent::SyncFinished(sync) if matches!(sync.origin, Origin::Connect(_)))
    })
    .await;
    let (LiveEvent::SyncFinished(first), LiveEvent::SyncFinished(then)) = (first, then) else {
        unreachable!("matched as finished syncs");
    };
    assert!(
        matches!(first.origin, Origin::Accept),
        "the exchange of first was not the accepted one: {:?}",
        first.origin
    );
    Ok((first, then))
}

/// `then` started no earlier than `first` finished. The finished stamp is
/// taken after the exchange releases its held dials; the slow egress keeps
/// the exchange itself over a second long.
fn assert_started_after(then: &SyncEvent, first: &SyncEvent) -> Result<()> {
    let released = first
        .finished
        .checked_sub(Duration::from_millis(100))
        .context("a finished stamp after the epoch")?;
    assert!(
        then.started >= released,
        "the held dial started {:?} before the exchange it follows finished",
        first
            .finished
            .duration_since(then.started)
            .unwrap_or_default()
    );
    Ok(())
}

/// A dial held behind an exchange stays held when the dial of that exchange
/// loses its slot to the counterpart's own and then ends refused: it starts
/// only once the exchange that took the slot over has finished.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn a_dial_superseded_by_the_tie_break_leaves_the_ordered_dial_held() -> Result<()> {
    let (first, then) = crossed_dials_of_first(
        b"a_dial_superseded_by_the_tie_break_leaves_the_ordered_dial_held",
        || SessionAccess::Deny,
    )
    .await?;
    assert_started_after(&then, &first)
}

/// A dial held behind an exchange stays held when the dial of that exchange
/// is refused by the counterpart as already syncing: it starts only once
/// the exchange the counterpart refused it for has finished.
///
/// `x`'s dial goes out once `y`'s is accepted, so `y`, the lesser id with
/// its own dial of `first` running, refuses it as already syncing.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn a_dial_refused_as_already_syncing_leaves_the_ordered_dial_held() -> Result<()> {
    let (first, then) = crossed_dials_of_first(
        b"a_dial_refused_as_already_syncing_leaves_the_ordered_dial_held",
        SessionAccess::whole,
    )
    .await?;
    assert_started_after(&then, &first)
}

/// A dial held behind an exchange that a resync is queued behind starts
/// only once the resync has finished, not when the exchange it followed
/// did: the sync of `first` asked for while its exchange runs queues the
/// resync, and the dial was for what that exchange may not carry.
///
/// `x`'s dial of `first` waits in its provider while the second sync is
/// asked for.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn a_dial_held_behind_an_exchange_with_a_resync_queued_follows_the_resync() -> Result<()> {
    let mut rng =
        test_rng(b"a_dial_held_behind_an_exchange_with_a_resync_queued_follows_the_resync");
    let first_id: Arc<std::sync::OnceLock<NamespaceId>> = Arc::default();
    let dial_asked = Arc::new(Notify::new());
    let go = Arc::new(Notify::new());
    let gate = Arc::new(AtomicBool::new(true));
    let x_access: SessionAccessProvider = {
        let (first_id, dial_asked, go) = (first_id.clone(), dial_asked.clone(), go.clone());
        Arc::new(move |namespace, _identity, _caller, _peer, role| {
            let held = role == SessionRole::Dial
                && first_id.get() == Some(&namespace)
                && gate.swap(false, Ordering::SeqCst);
            let (dial_asked, go) = (dial_asked.clone(), go.clone());
            Box::pin(async move {
                if held {
                    dial_asked.notify_one();
                    go.notified().await;
                }
                SessionAccess::whole()
            })
        })
    };
    let x = test_node(SecretKey::from_bytes(&rng.random()))
        .await?
        .access(x_access)
        .spawn()
        .await?;
    let y = test_node(SecretKey::from_bytes(&rng.random()))
        .await?
        .spawn()
        .await?;
    let id_y = y.id();

    let (first_x, then_x) = (x.docs().create().await?, x.docs().create().await?);
    first_id
        .set(first_x.id())
        .map_err(|_| anyhow!("first set once"))?;
    let mut y_contacts = Vec::new();
    for doc in [&first_x, &then_x] {
        let ticket = doc.share(AddrInfoOptions::RelayAndAddresses).await?;
        let imported = y.docs().import_namespace(ticket.capability).await?;
        imported
            .sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
            .await?;
        y_contacts = imported
            .share(AddrInfoOptions::RelayAndAddresses)
            .await?
            .contacts();
    }
    let mut first_events = first_x.subscribe().await?;
    let mut then_events = then_x.subscribe().await?;

    x.engine()
        .order_after(then_x.id(), Some(first_x.id()))
        .await?;
    first_x
        .sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
        .await?;
    then_x
        .sync_with_peers(y_contacts.clone(), vec![], util::TEST_HOLDER, false)
        .await?;
    n0_future::time::timeout(TIMEOUT, dial_asked.notified()).await?;
    // Asked for while the exchange of `first` runs: queued as its resync.
    first_x
        .sync_with_peers(y_contacts, vec![], util::TEST_HOLDER, false)
        .await?;
    go.notify_one();

    let resync = next_event_matching(&mut first_events, TIMEOUT, move |e| {
        match_sync_finished(e, id_y)
            && matches!(e, LiveEvent::SyncFinished(sync) if sync.origin == Origin::Connect(SyncReason::Resync))
    })
    .await;
    let then = next_event_matching(&mut then_events, TIMEOUT, move |e| {
        match_sync_finished(e, id_y)
            && matches!(e, LiveEvent::SyncFinished(sync) if matches!(sync.origin, Origin::Connect(_)))
    })
    .await;
    let (LiveEvent::SyncFinished(resync), LiveEvent::SyncFinished(then)) = (resync, then) else {
        unreachable!("matched as finished syncs");
    };
    assert!(
        then.started > resync.started,
        "the held dial started {:?} before the resync it was left to",
        resync
            .started
            .duration_since(then.started)
            .unwrap_or_default()
    );
    Ok(())
}

/// A sync handed its peers dials those alone, none of the other peers the
/// replica recorded; the recorded peer left out is dialed once it is handed.
///
/// Every replica stays outside the swarm and nothing is written, so a
/// session reaches the peer left out only through a dial of that sync. The
/// peer left out sees its own first session end before the window opens:
/// its end of that session can come after node 0's.
#[tokio::test]
#[traced_test]
async fn a_sync_with_handed_peers_dials_no_other_recorded_peer() -> Result<()> {
    let mut rng = test_rng(b"a_sync_with_handed_peers_dials_no_other_recorded_peer");
    let nodes = spawn_nodes(3, &mut rng).await?;
    let (id0, id1, id2) = (nodes[0].id(), nodes[1].id(), nodes[2].id());
    let doc0 = nodes[0].docs().create().await?;
    doc0.sync_with_peers(vec![], vec![], util::TEST_HOLDER, false)
        .await?;
    let ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;
    let mut events0 = doc0.subscribe().await?;
    let mut events = Vec::new();
    for node in &nodes[1..] {
        let doc = node
            .docs()
            .import_namespace(ticket.capability.clone())
            .await?;
        events.push(doc.subscribe().await?);
        doc.sync_with_peers(ticket.contacts(), vec![], util::TEST_HOLDER, false)
            .await?;
    }
    // Both first sessions finished, so node 0 recorded both peers.
    assert_events_matching(
        &mut events0,
        TIMEOUT,
        vec![
            Box::new(move |e| match_sync_finished(e, id1)),
            Box::new(move |e| match_sync_finished(e, id2)),
        ],
    )
    .await;
    let mut events2 = events.pop().context("node 2 subscribed")?;
    next_event_matching(&mut events2, TIMEOUT, move |e| match_sync_finished(e, id0)).await;

    doc0.sync_with_peers(vec![], vec![*id1.as_bytes()], util::TEST_HOLDER, false)
        .await?;
    next_event_matching(&mut events0, TIMEOUT, move |e| match_sync_finished(e, id1)).await;
    let reached_two = n0_future::time::timeout(
        Duration::from_secs(1),
        next_event_matching(
            &mut events2,
            TIMEOUT,
            move |e| matches!(e, LiveEvent::SyncFinished(sync) if sync.peer == id0),
        ),
    )
    .await;
    assert!(
        reached_two.is_err(),
        "a recorded peer not handed was dialed"
    );

    doc0.sync_with_peers(vec![], vec![*id2.as_bytes()], util::TEST_HOLDER, false)
        .await?;
    next_event_matching(&mut events2, TIMEOUT, move |e| match_sync_finished(e, id0)).await;
    Ok(())
}

/// A record can arrive from a peer that does not have the record's content:
/// the sender is a relay that received the record but never fetched the
/// bytes. The receiver then has no provider to download from, and the
/// content hash is parked. Every successful sync retries the namespace's
/// parked hashes against the just-synced peer, so the first sync with a
/// peer that has the content delivers it; without that retry the hash
/// waits on a best-effort gossip `ContentReady` broadcast that may never
/// come, while sync exchanges with peers holding the bytes go on.
///
/// The writer serves no payload, so the relay's fetch from it fails, and it
/// is gone before the receiver joins, so the record reaches the receiver
/// through the relay alone. The bytes then come from a holder that took
/// them outside the replica.
#[tokio::test]
#[traced_test]
async fn sync_fetches_parked_content_from_later_sync_peer() -> Result<()> {
    let mut rng = test_rng(b"sync_fetches_parked_content_from_later_sync_peer");

    // Four nodes on loopback: the test drives every exchange explicitly
    // and must not depend on the host's external interfaces.
    let mut nodes = Vec::new();
    for at in 0..4 {
        let secret_key = SecretKey::from_bytes(&rng.random());
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .bind_addr("127.0.0.1:0")?
            .bind()
            .await?;
        let node = Node::memory(ep);
        let node = if at == 0 {
            node.without_blob_serving()
        } else {
            node
        };
        nodes.push(node.spawn().await?);
    }
    let writer = nodes.remove(0);
    let (relay, receiver, holder) = (&nodes[0], &nodes[1], &nodes[2]);
    let writer_id = writer.id();
    let (relay_id, holder_id) = (relay.id(), holder.id());

    let author = writer.docs().author_create().await?;
    let doc_writer = writer.docs().create().await?;
    let hash = doc_writer
        .set_bytes(author, b"k".to_vec(), b"v".to_vec())
        .await?;
    let ticket_writer = doc_writer.share(AddrInfoOptions::RelayAndAddresses).await?;

    // The relay receives the record but never the content.
    let doc_relay = relay
        .docs()
        .import_namespace(ticket_writer.capability.clone())
        .await?;
    let mut events_relay = doc_relay.subscribe().await?;
    doc_relay
        .start_sync(ticket_writer.contacts(), util::TEST_HOLDER)
        .await?;
    next_event_matching(
        &mut events_relay,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == writer_id),
    )
    .await;
    drop(doc_writer);
    writer.shutdown().await?;
    assert!(
        !relay.blobs().has(hash).await?,
        "a writer that serves no payload must leave the relay content-less"
    );

    // The receiver learns the record from the relay — the only live peer —
    // so the record arrives with the content missing at its sender, and the
    // content hash parks with no usable provider.
    let ticket_relay = doc_relay.share(AddrInfoOptions::RelayAndAddresses).await?;
    let doc_receiver = receiver
        .docs()
        .import_namespace(ticket_relay.capability.clone())
        .await?;
    let mut events_receiver = doc_receiver.subscribe().await?;
    doc_receiver
        .start_sync(ticket_relay.contacts(), util::TEST_HOLDER)
        .await?;
    next_event_matching(&mut events_receiver, TIMEOUT, |e| {
        matches!(
            e,
            LiveEvent::InsertRemote { from, content_status: ContentStatus::Missing, .. }
                if *from == relay_id
        )
    })
    .await;
    assert!(
        !receiver.blobs().has(hash).await?,
        "the receiver cannot have content its only peer does not hold"
    );

    // The holder takes the bytes outside the replica, and the receiver
    // completes a sync with it. The sync carries the record the other way
    // alone, so nothing but the finished sync's retry of the parked hash
    // asks the holder for the bytes.
    let held = holder.blobs().add_bytes(b"v".to_vec()).await?;
    assert_eq!(held.hash, hash);
    let doc_holder = holder
        .docs()
        .import_namespace(ticket_writer.capability.clone())
        .await?;
    doc_holder.start_sync(vec![], util::TEST_HOLDER).await?;
    doc_receiver
        .start_sync(
            doc_holder
                .ticket(AddrInfoOptions::RelayAndAddresses)
                .await?
                .contacts(),
            util::TEST_HOLDER,
        )
        .await?;
    next_event_matching(&mut events_receiver, TIMEOUT, |e| {
        match_sync_finished(e, holder_id)
    })
    .await;
    let deadline = Instant::now() + TIMEOUT;
    while !receiver.blobs().has(hash).await? {
        assert!(
            Instant::now() < deadline,
            "parked content did not arrive from a later sync peer"
        );
        n0_future::time::sleep(Duration::from_millis(100)).await;
    }
    assert_latest(receiver.blobs(), &doc_receiver, b"k", b"v").await;

    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// Content a record arrived without before its node restarted is fetched
/// from the first peer a session goes through with after the restart. The
/// engine parks such content in memory, and a session after the restart
/// exchanges no record, so nothing else asks for it again.
///
/// The record reaches the receiver through a relay a writer serving no
/// payload left content-less, as in the test above, and the holder of the
/// bytes appears after the receiver has restarted. The receiver stops with
/// its download of the content in flight, so a node stopping its router
/// before its engine panics on the blob store's thread, counted here.
#[tokio::test]
#[traced_test]
async fn content_a_restart_left_missing_is_fetched_from_the_first_sync_peer() -> Result<()> {
    let panics = util::count_panics();
    let mut rng = test_rng(b"content_a_restart_left_missing_is_fetched_from_the_first_sync_peer");
    let loopback = |secret_key: SecretKey| async move {
        Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .bind_addr("127.0.0.1:0")?
            .bind()
            .await
            .map_err(anyhow::Error::from)
    };
    let writer = Node::memory(loopback(SecretKey::from_bytes(&rng.random())).await?)
        .without_blob_serving()
        .spawn()
        .await?;
    let relay = Node::memory(loopback(SecretKey::from_bytes(&rng.random())).await?)
        .spawn()
        .await?;
    let holder = Node::memory(loopback(SecretKey::from_bytes(&rng.random())).await?)
        .spawn()
        .await?;
    let receiver_dir = tempdir()?;
    let receiver_key = SecretKey::from_bytes(&rng.random());
    let receiver = Node::persistent(&receiver_dir, loopback(receiver_key.clone()).await?)
        .spawn()
        .await?;
    let (writer_id, relay_id, holder_id) = (writer.id(), relay.id(), holder.id());

    let author = writer.docs().author_create().await?;
    let doc_writer = writer.docs().create().await?;
    let hash = doc_writer
        .set_bytes(author, b"k".to_vec(), b"v".to_vec())
        .await?;
    let ticket_writer = doc_writer.share(AddrInfoOptions::RelayAndAddresses).await?;
    let doc_relay = relay
        .docs()
        .import_namespace(ticket_writer.capability.clone())
        .await?;
    let mut events_relay = doc_relay.subscribe().await?;
    doc_relay
        .start_sync(ticket_writer.contacts(), util::TEST_HOLDER)
        .await?;
    next_event_matching(
        &mut events_relay,
        TIMEOUT,
        |e| matches!(e, LiveEvent::InsertRemote { from, .. } if *from == writer_id),
    )
    .await;
    drop(doc_writer);
    writer.shutdown().await?;

    let ticket_relay = doc_relay.share(AddrInfoOptions::RelayAndAddresses).await?;
    let doc_receiver = receiver
        .docs()
        .import_namespace(ticket_relay.capability.clone())
        .await?;
    let mut events_receiver = doc_receiver.subscribe().await?;
    doc_receiver
        .start_sync(ticket_relay.contacts(), util::TEST_HOLDER)
        .await?;
    next_event_matching(&mut events_receiver, TIMEOUT, |e| {
        matches!(
            e,
            LiveEvent::InsertRemote { from, content_status: ContentStatus::Missing, .. }
                if *from == relay_id
        )
    })
    .await;
    let namespace = doc_receiver.id();
    drop((doc_receiver, events_receiver));
    receiver.shutdown().await?;

    let receiver = Node::persistent(&receiver_dir, loopback(receiver_key).await?)
        .spawn()
        .await?;
    assert!(
        !receiver.blobs().has(hash).await?,
        "the receiver held the content before the restart"
    );
    let doc_receiver = receiver
        .docs()
        .open(namespace)
        .await?
        .context("the receiver lost the replica in the restart")?;
    let mut events_receiver = doc_receiver.subscribe().await?;
    let held = holder.blobs().add_bytes(b"v".to_vec()).await?;
    assert_eq!(held.hash, hash);
    let doc_holder = holder
        .docs()
        .import_namespace(ticket_writer.capability.clone())
        .await?;
    doc_holder.start_sync(vec![], util::TEST_HOLDER).await?;
    doc_receiver
        .start_sync(
            doc_holder
                .ticket(AddrInfoOptions::RelayAndAddresses)
                .await?
                .contacts(),
            util::TEST_HOLDER,
        )
        .await?;
    next_event_matching(&mut events_receiver, TIMEOUT, |e| {
        match_sync_finished(e, holder_id)
    })
    .await;
    let deadline = Instant::now() + TIMEOUT;
    while !receiver.blobs().has(hash).await? {
        assert!(
            Instant::now() < deadline,
            "content the restart left missing did not arrive from the holder"
        );
        n0_future::time::sleep(Duration::from_millis(100)).await;
    }
    assert_latest(receiver.blobs(), &doc_receiver, b"k", b"v").await;

    for node in [relay, holder, receiver] {
        node.shutdown().await?;
    }
    assert_eq!(
        panics.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a thread panicked while the nodes ran or stopped"
    );
    Ok(())
}

/// A subscriber that stops reading holds up neither the replica nor the
/// live engine: the importing node takes in more entries and their content
/// than its unread subscription buffers, and every one becomes readable.
///
/// Six hundred entries of distinct content: each emits an insert and a
/// content event, and a subscription buffers 256 of each. The importing node
/// asks for a session every two seconds while it waits, as a reconcile pass
/// does: a download that fails is retried only on a later session.
#[tokio::test]
async fn an_unread_subscription_holds_up_no_sync() -> Result<()> {
    const ENTRIES: usize = 600;
    let mut rng = test_rng(b"an_unread_subscription_holds_up_no_sync");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    let ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;
    let writer = util::contacts(ticket.nodes.clone());
    let doc1 = util::import(clients[1].docs(), ticket).await?;
    let _unread = doc1.subscribe().await?;

    for n in 0..ENTRIES {
        doc0.set_bytes(author0, format!("k{n:04}"), format!("v{n:04}"))
            .await?;
    }
    let blobs1 = clients[1].blobs();
    let taken_in = async {
        for n in 0..ENTRIES {
            let key = format!("k{n:04}");
            let value = format!("v{n:04}");
            while get_latest(blobs1, &doc1, key.as_bytes())
                .await
                .ok()
                .as_deref()
                != Some(value.as_bytes())
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let asking = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _next_pass = doc1.start_sync(writer.clone(), util::TEST_HOLDER).await;
        }
    };
    let waited = tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            () = taken_in => {}
            () = asking => {}
        }
    })
    .await;
    if waited.is_err() {
        let counted = tokio::time::timeout(Duration::from_secs(10), async {
            let (mut entries, mut readable) = (0, 0);
            for n in 0..ENTRIES {
                let key = format!("k{n:04}");
                if get_latest(blobs1, &doc1, key.as_bytes()).await.is_ok() {
                    (entries, readable) = (entries + 1, readable + 1);
                } else if get_exact(&doc1, author0, key, false).await?.is_some() {
                    entries += 1;
                }
            }
            anyhow::Ok((entries, readable))
        })
        .await;
        match counted {
            Ok(Ok((entries, readable))) => bail!(
                "the importing node stopped taking in entries past its unread subscription: \
                 {entries} of {ENTRIES} entries, {readable} readable"
            ),
            _ => bail!("the importing node's store stopped answering"),
        }
    }

    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}

/// A catch-up of more entries than the live actor's events channel holds
/// finishes while the live actor keeps asking the sync actor for the
/// replica's recorded peers.
///
/// The importing node holds nothing, so the whole store arrives in one
/// message the sync actor inserts in one step; a restart every few
/// milliseconds puts the live actor's request behind that step.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_past_the_events_channel_finishes_while_the_live_actor_asks_the_sync_actor(
) -> Result<()> {
    const ENTRIES: usize = 2_000;
    let mut rng = test_rng(b"a_catch_up_past_the_events_channel_finishes");
    let nodes = spawn_nodes(2, &mut rng).await?;
    let clients = nodes.iter().map(|node| node.client()).collect::<Vec<_>>();

    let author0 = clients[0].docs().author_create().await?;
    let doc0 = clients[0].docs().create().await?;
    for n in 0..ENTRIES {
        doc0.set_bytes(author0, format!("k{n:04}"), format!("v{n:04}"))
            .await?;
    }
    let ticket = doc0.share(AddrInfoOptions::RelayAndAddresses).await?;
    let doc1 = util::import(clients[1].docs(), ticket).await?;

    let caught_up = async {
        while get_all(&doc1).await?.len() < ENTRIES {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::Ok(())
    };
    let asking = async {
        loop {
            if let Err(err) = doc1.start_sync(Vec::new(), util::TEST_HOLDER).await {
                break Err::<(), _>(err);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            res = caught_up => res,
            res = asking => res.context("a restart failed"),
        }
    })
    .await
    .context("the importing node's live and sync actors stopped answering")??;

    for node in nodes {
        node.shutdown().await?;
    }
    Ok(())
}
