//! The ordered subscription (the data layer's `change-subscription` spec):
//! one replica's events in the order the live actor handled them, at the
//! capacity its caller names, and the session id both ends of a session
//! report.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use iroh::{endpoint::presets, protocol::Router, Endpoint, Watcher as _};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, Hash};
use iroh_gossip::net::Gossip;
use n0_future::{Stream, StreamExt};
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc, DocsApi,
    },
    engine::{LiveEvent, Origin, SyncEvent},
    filter::serve_whole,
    protocol::Docs,
    Contact, ContentStatus, Identity, NamespaceId,
};

const HOLDER: Identity = Identity::from_bytes([0u8; 32]);
const WORK: Identity = Identity::from_bytes([0xa2; 32]);
const LEISURE: Identity = Identity::from_bytes([0xa3; 32]);

/// What the runtime's observation asks for per store.
const CAPACITY: usize = 4_096;
/// A session between two endpoints on loopback, 1,000 entries at most.
const TIMEOUT: Duration = Duration::from_secs(60);

/// One node of one identity on loopback, its blob store kept for the
/// scenario to inspect.
struct Peer {
    router: Router,
    docs: Docs,
    blobs: MemStore,
}

impl Peer {
    async fn spawn() -> Result<Self> {
        let endpoint = loopback_endpoint().await?;
        let blobs = MemStore::new();
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory(HOLDER, serve_whole())
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::ALPN, gossip)
            .accept(pdn_store::ALPN, docs.clone())
            .spawn();
        Ok(Self {
            router,
            docs,
            blobs,
        })
    }

    fn api(&self) -> &DocsApi {
        self.docs.api()
    }

    fn contacts(&self) -> Vec<Contact> {
        vec![Contact::new(self.router.endpoint().addr(), HOLDER)]
    }

    async fn holds(&self, hash: Hash) -> Result<bool> {
        Ok(self.blobs.has(hash).await?)
    }
}

/// An endpoint publishes its first address a moment after it binds, and a
/// contact taken before that names no path.
async fn loopback_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0u16))?
        .bind()
        .await?;
    while endpoint.watch_addr().get().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(endpoint)
}

/// A writer holding `count` entries `{prefix}{i}`, each with a payload of
/// its own, and a reader holding the namespace and nothing of it. The reader
/// joins no swarm, so the two are never neighbors and the only sessions are
/// the ones a scenario dials.
async fn writer_and_reader(count: usize, prefix: &str) -> Result<(Peer, Doc, Peer, Doc)> {
    let writer = Peer::spawn().await?;
    let reader = Peer::spawn().await?;
    let written = writer.api().create().await?;
    write(&writer, &written, prefix, 0..count).await?;
    written.start_sync_scoped(vec![], HOLDER).await?;
    let ticket = written.share(ShareMode::Write, AddrInfoOptions::Id).await?;
    let read = reader.api().import_namespace(ticket.capability).await?;
    Ok((writer, written, reader, read))
}

async fn write(
    peer: &Peer,
    doc: &Doc,
    prefix: &str,
    range: std::ops::Range<usize>,
) -> Result<Vec<Hash>> {
    let author = peer.api().author_default().await?;
    let mut hashes = Vec::new();
    for i in range {
        let key = format!("{prefix}{i}");
        hashes.push(doc.set_bytes(author, key.clone(), key).await?);
    }
    Ok(hashes)
}

/// Events up to and including the first `last` takes; panics when the
/// stream ends or stays quiet past [`TIMEOUT`] first.
async fn read_until(
    events: &mut (impl Stream<Item = Result<LiveEvent>> + Unpin),
    mut last: impl FnMut(&LiveEvent) -> bool,
) -> Result<Vec<LiveEvent>> {
    let mut read = Vec::new();
    loop {
        let event = tokio::time::timeout(TIMEOUT, events.next())
            .await
            .with_context(|| format!("quiet after {} events", read.len()))?
            .with_context(|| format!("ended after {} events", read.len()))??;
        let done = last(&event);
        read.push(event);
        if done {
            return Ok(read);
        }
    }
}

async fn wait_for(
    events: &mut (impl Stream<Item = Result<LiveEvent>> + Unpin),
    wanted: impl FnMut(&LiveEvent) -> bool,
) -> Result<LiveEvent> {
    let mut read = read_until(events, wanted).await?;
    read.pop().context("read_until returns what it matched")
}

async fn wait_until_held(peer: &Peer, hashes: &[Hash]) -> Result<()> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    for hash in hashes {
        while !peer.holds(*hash).await? {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "a payload never arrived"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    Ok(())
}

fn finished(event: &LiveEvent) -> Option<&SyncEvent> {
    match event {
        LiveEvent::SyncFinished(session) => Some(session),
        _ => None,
    }
}

fn finished_ok(event: &LiveEvent) -> bool {
    finished(event).is_some_and(|session| session.result.is_ok())
}

/// After the first finished session: the downloads it queued have ended.
fn downloads_ended_after_a_session() -> impl FnMut(&LiveEvent) -> bool {
    let mut seen_end = false;
    move |event| {
        seen_end |= finished_ok(event);
        seen_end && matches!(event, LiveEvent::PendingContentReady)
    }
}

fn remote_key(event: &LiveEvent) -> Option<&[u8]> {
    match event {
        LiveEvent::InsertRemote { entry, .. } => Some(entry.key()),
        _ => None,
    }
}

fn position(events: &[LiveEvent], wanted: impl Fn(&LiveEvent) -> bool) -> Option<usize> {
    events.iter().position(wanted)
}

/// A session delivering 1,000 entries to a subscriber that reads nothing
/// until it and its downloads have ended reports every insert before the
/// session's end, and loses nothing.
///
/// Fails with the ordered subscription replaced by `Engine::subscribe`, or
/// with a capacity of 256: either buffer drops what the reader left unread.
#[tokio::test(flavor = "multi_thread")]
async fn a_thousand_entry_session_is_reported_entries_first() -> Result<()> {
    const ENTRIES: usize = 1_000;
    let (writer, written, reader, read) = writer_and_reader(ENTRIES, "k").await?;
    let hashes: Vec<Hash> = (0..ENTRIES).map(|i| Hash::new(format!("k{i}"))).collect();
    read.start_sync_scoped(vec![], HOLDER).await?;
    let mut observed = read.observe(CAPACITY).await?;
    let mut product = read.subscribe().await?;
    read.start_sync_scoped(writer.contacts(), HOLDER).await?;

    // Sentinels: the session ended and every payload arrived, so the
    // subscriber has read nothing across the whole session.
    wait_for(&mut product, finished_ok).await?;
    wait_until_held(&reader, &hashes).await?;

    let events = read_until(&mut observed, downloads_ended_after_a_session()).await?;
    assert!(
        !events.iter().any(|e| matches!(e, LiveEvent::Lagged)),
        "a subscriber at 4,096 lagged on a 1,000-entry session"
    );
    let end = position(&events, finished_ok).context("the session's end")?;
    let inserts = events
        .get(..end)
        .context("the events before the end")?
        .iter()
        .filter_map(remote_key)
        .collect::<HashSet<_>>();
    assert_eq!(
        inserts.len(),
        ENTRIES,
        "not every insert came before the session's end"
    );
    drop(written);
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// A download's completion is reported after the insert that asked for it.
///
/// Fails with the ordered subscription replaced by `Engine::subscribe`,
/// whose two merged channels hand a reader that comes late the live
/// actor's events first whenever the insert's conversion is still pending.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_completes_after_its_insert() -> Result<()> {
    let (writer, _written, reader, read) = writer_and_reader(1, "k").await?;
    let hash = Hash::new("k0");
    read.start_sync_scoped(vec![], HOLDER).await?;
    // The engine's own: the API's forwarding reads as events come, so a
    // reader behind it never comes late.
    let mut observed = reader.docs.engine().observe(read.id(), CAPACITY).await?;
    let mut product = read.subscribe().await?;
    read.start_sync_scoped(writer.contacts(), HOLDER).await?;
    wait_for(&mut product, finished_ok).await?;
    wait_until_held(&reader, &[hash]).await?;

    let events = read_until(&mut observed, downloads_ended_after_a_session()).await?;
    let insert = position(&events, |e| remote_key(e) == Some(b"k0")).context("the insert")?;
    let ready = position(
        &events,
        |e| matches!(e, LiveEvent::ContentReady { hash: h } if *h == hash),
    )
    .context("the download's completion")?;
    assert!(
        insert < ready,
        "the download completed before its insert: {events:?}"
    );
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// An ordered subscription to a replica the live sync does not hold open is
/// refused; once its sync runs, the same request is served.
///
/// Fails with the actor's check that the namespace syncs removed.
#[tokio::test]
async fn a_replica_not_held_open_for_sync_is_refused() -> Result<()> {
    let peer = Peer::spawn().await?;
    let doc = peer.api().create().await?;
    assert!(
        doc.observe(CAPACITY).await.is_err(),
        "an ordered subscription opened on a replica not syncing"
    );
    assert!(
        peer.docs
            .engine()
            .observe(doc.id(), CAPACITY)
            .await
            .is_err(),
        "the engine opened an ordered subscription on a replica not syncing"
    );
    doc.start_sync_scoped(vec![], HOLDER).await?;
    let _observed = doc.observe(CAPACITY).await?;
    peer.router.shutdown().await?;
    Ok(())
}

/// A dropped replica ends its ordered subscription after the events
/// reported before the drop, and a later write of the peer, dialed to this
/// node, reaches no subscriber of it.
///
/// Fails with `leave` keeping the ordered subscribers it kills.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_replica_ends_its_ordered_subscription() -> Result<()> {
    let (writer, written, reader, read) = writer_and_reader(1, "k").await?;
    let namespace: NamespaceId = read.id();
    read.start_sync_scoped(vec![], HOLDER).await?;
    let mut observed = read.observe(CAPACITY).await?;
    let mut product = read.subscribe().await?;
    read.start_sync_scoped(writer.contacts(), HOLDER).await?;
    wait_for(&mut product, finished_ok).await?;

    reader.api().drop_doc(namespace).await?;
    let mut before_drop = Vec::new();
    while let Some(event) = tokio::time::timeout(TIMEOUT, observed.next())
        .await
        .context("the subscription outlived its replica")?
    {
        before_drop.push(event?);
    }
    assert!(
        before_drop.iter().any(|e| remote_key(e) == Some(b"k0")),
        "the insert reported before the drop was lost"
    );

    // Paired denial: the peer's next write, dialed here, is refused, and no
    // new subscription to the namespace opens.
    let mut writer_events = written.subscribe().await?;
    write(&writer, &written, "late", 0..1).await?;
    written.start_sync_scoped(reader.contacts(), HOLDER).await?;
    // The writer's end of the earlier session can still arrive here.
    let refused = wait_for(&mut writer_events, |e| {
        finished(e).is_some_and(|s| matches!(s.origin, Origin::Connect(_)))
    })
    .await?;
    assert!(
        !finished_ok(&refused),
        "a dropped replica served a session: {refused:?}"
    );
    assert!(
        reader
            .docs
            .engine()
            .observe(namespace, CAPACITY)
            .await
            .is_err(),
        "a dropped replica took an ordered subscription"
    );
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// A subscriber that reads nothing while more events than its capacity are
/// produced receives the events that fit and then one lag notice, and the
/// session it watches completes in time.
///
/// Fails with the ordered subscription delivered `Blocking`: the live actor
/// waits on the full channel and the session never finishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_past_its_capacity_is_told_and_holds_up_nothing() -> Result<()> {
    const SMALL: usize = 16;
    let (writer, _written, reader, read) = writer_and_reader(100, "k").await?;
    read.start_sync_scoped(vec![], HOLDER).await?;
    // The engine's own: the API holds events of its own in front of it.
    let mut observed = reader.docs.engine().observe(read.id(), SMALL).await?;
    let mut product = read.subscribe().await?;
    read.start_sync_scoped(writer.contacts(), HOLDER).await?;
    wait_for(&mut product, finished_ok).await?;

    let mut kept = Vec::new();
    for _ in 0..SMALL {
        let event = tokio::time::timeout(TIMEOUT, observed.next())
            .await?
            .context("the subscription ended")??;
        kept.push(event);
    }
    let (last, fitted) = kept.split_last().context("events were read")?;
    assert!(
        matches!(last, LiveEvent::Lagged),
        "no lag notice after the events that fit: {kept:?}"
    );
    assert!(
        !fitted.iter().any(|e| matches!(e, LiveEvent::Lagged)),
        "a lag notice before the buffer was full"
    );
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// A session between two identities of one node is reported in the same
/// order as one over the network: every insert before its end, which names
/// the node itself as the counterpart and carries no session id.
///
/// Fails with the ordered subscription fed from two merged channels
/// (`Engine::subscribe`), read once the session has ended.
#[tokio::test(flavor = "multi_thread")]
async fn an_in_process_session_is_reported_in_the_same_order() -> Result<()> {
    const ENTRIES: usize = 300;
    let endpoint = loopback_endpoint().await?;
    let blobs = MemStore::new();
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let work = Docs::memory(WORK, serve_whole())
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let leisure = Docs::memory(LEISURE, serve_whole())
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let written = work.api().create().await?;
    let author = work.api().author_default().await?;
    for i in 0..ENTRIES {
        written
            .set_bytes(author, format!("k{i}"), format!("k{i}"))
            .await?;
    }
    written.start_sync_scoped(vec![], WORK).await?;
    let ticket = written.share(ShareMode::Read, AddrInfoOptions::Id).await?;
    let read = leisure.api().import_namespace(ticket.capability).await?;
    read.start_sync_scoped(vec![], LEISURE).await?;
    let mut observed = leisure.engine().observe(read.id(), CAPACITY).await?;
    let mut product = read.subscribe().await?;

    leisure
        .engine()
        .sync_in_process(work.engine(), read.id(), WORK)
        .await?;
    wait_for(&mut product, finished_ok).await?;
    let events = read_until(&mut observed, finished_ok).await?;
    let inserts = events.iter().filter_map(remote_key).count();
    assert_eq!(
        inserts, ENTRIES,
        "not every insert came before the session's end"
    );
    let end = events
        .last()
        .and_then(finished)
        .context("the session's end")?;
    assert_eq!(end.peer, endpoint.id(), "the counterpart is not this node");
    assert_eq!(end.session, None, "an in-process session carried an id");
    work.engine().shutdown().await?;
    leisure.engine().shutdown().await?;
    gossip.shutdown().await?;
    blobs.shutdown().await?;
    endpoint.close().await;
    Ok(())
}

/// An ordered subscription taken with the start of sync reports every
/// insert of the reader's first session before that session's end.
///
/// Fails with the subscription left off the start and taken by `observe`
/// once the first session has finished.
#[tokio::test(flavor = "multi_thread")]
async fn an_ordered_subscription_taken_with_the_start_reports_the_first_session() -> Result<()> {
    const ENTRIES: usize = 300;
    let (writer, _written, reader, read) = writer_and_reader(ENTRIES, "k").await?;
    let mut observed = read
        .start_sync_observed(writer.contacts(), HOLDER, CAPACITY)
        .await?;
    let events = read_until(&mut observed, |e| finished(e).is_some()).await?;
    assert!(
        events.last().is_some_and(finished_ok),
        "the first session failed"
    );
    let inserts = events.iter().filter_map(remote_key).collect::<HashSet<_>>();
    assert_eq!(
        inserts.len(),
        ENTRIES,
        "the first session's inserts were not all reported before its end"
    );
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// Two consecutive sessions of one pair, the first dialed by the reader and
/// the second accepted from the writer: every insert of the second comes
/// after the first's end, for a subscriber that reads only after both.
///
/// Fails with the ordered subscription fed from two merged channels
/// (`Engine::subscribe`), which order a late reader's events by the channel
/// that is ready, not by when they happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairs_later_session_follows_the_end_of_its_earlier_one() -> Result<()> {
    const BATCH: usize = 50;
    let (writer, written, reader, read) = writer_and_reader(BATCH, "first").await?;
    read.start_sync_scoped(vec![], HOLDER).await?;
    // The engine's own: the API's forwarding reads as events come, so a
    // reader behind it never comes late.
    let mut observed = reader.docs.engine().observe(read.id(), CAPACITY).await?;
    let mut product = read.subscribe().await?;

    read.start_sync_scoped(writer.contacts(), HOLDER).await?;
    wait_for(&mut product, |e| {
        finished(e).is_some_and(|s| s.result.is_ok() && matches!(s.origin, Origin::Connect(_)))
    })
    .await?;

    write(&writer, &written, "second", 0..BATCH).await?;
    written.start_sync_scoped(reader.contacts(), HOLDER).await?;
    let carried = |e: &LiveEvent| {
        finished(e).is_some_and(|s| {
            matches!(s.origin, Origin::Accept)
                && s.result
                    .as_ref()
                    .is_ok_and(|details| details.entries_received > 0)
        })
    };
    wait_for(&mut product, carried).await?;

    let events = read_until(&mut observed, carried).await?;
    let first_end = position(&events, |e| {
        finished(e).is_some_and(|s| matches!(s.origin, Origin::Connect(_)))
    })
    .context("the first session's end")?;
    let second = events
        .iter()
        .enumerate()
        .filter(|(_, e)| remote_key(e).is_some_and(|key| key.starts_with(b"second")))
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    assert_eq!(
        second.len(),
        BATCH,
        "the second session's inserts are missing"
    );
    assert!(
        second.iter().all(|at| *at > first_end),
        "an insert of the later session came before the earlier one's end"
    );
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// Of 3 entries arriving in one session, 2 sharing a payload and 1 carrying
/// a payload the reader held before, one completed download is reported,
/// for the shared payload, and the third's insert reports its content as
/// complete.
///
/// Fails with a download queued per entry, held or already queued.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_payload_downloads_once_and_a_held_one_not_at_all() -> Result<()> {
    let writer = Peer::spawn().await?;
    let reader = Peer::spawn().await?;
    let written = writer.api().create().await?;
    let author = writer.api().author_default().await?;
    let shared = written.set_bytes(author, "a", "shared").await?;
    written.set_bytes(author, "b", "shared").await?;
    let held = written.set_bytes(author, "c", "held").await?;
    written.start_sync_scoped(vec![], HOLDER).await?;
    reader.blobs.add_bytes("held").await?;
    let ticket = written.share(ShareMode::Read, AddrInfoOptions::Id).await?;
    let read = reader.api().import_namespace(ticket.capability).await?;
    read.start_sync_scoped(vec![], HOLDER).await?;
    let mut observed = read.observe(CAPACITY).await?;
    let mut product = read.subscribe().await?;
    read.start_sync_scoped(writer.contacts(), HOLDER).await?;
    wait_for(&mut product, finished_ok).await?;
    wait_until_held(&reader, &[shared]).await?;

    let events = read_until(&mut observed, downloads_ended_after_a_session()).await?;
    let downloads = events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::ContentReady { hash } => Some(*hash),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        downloads,
        vec![shared],
        "not exactly one download, of the shared payload"
    );
    let third = events
        .iter()
        .find_map(|e| match e {
            LiveEvent::InsertRemote {
                entry,
                content_status,
                ..
            } if entry.content_hash() == held => Some(*content_status),
            _ => None,
        })
        .context("the third entry's insert")?;
    assert_eq!(third, ContentStatus::Complete);
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}

/// Both sides of a session carry one session id, the reader's count
/// received equals the writer's count sent, and the pair's next session
/// carries another id.
///
/// Fails with the id taken from `stable_id`, which each end numbers on its
/// own.
#[tokio::test(flavor = "multi_thread")]
async fn both_sides_of_a_session_carry_one_session_id() -> Result<()> {
    let (writer, written, reader, read) = writer_and_reader(3, "k").await?;
    read.start_sync_scoped(vec![], HOLDER).await?;
    let mut reader_events = read.subscribe().await?;
    let mut writer_events = written.subscribe().await?;
    let dialed = |e: &LiveEvent| {
        finished(e).is_some_and(|s| s.result.is_ok() && matches!(s.origin, Origin::Connect(_)))
    };
    let accepted = |e: &LiveEvent| {
        finished(e).is_some_and(|s| s.result.is_ok() && matches!(s.origin, Origin::Accept))
    };

    let mut ids = Vec::new();
    for round in 0..2 {
        if round > 0 {
            write(&writer, &written, "more", 0..2).await?;
        }
        read.start_sync_scoped(writer.contacts(), HOLDER).await?;
        let reader_side = wait_for(&mut reader_events, dialed).await?;
        let writer_side = wait_for(&mut writer_events, accepted).await?;
        let (reader_side, writer_side) = (
            finished(&reader_side).context("matched as finished")?,
            finished(&writer_side).context("matched as finished")?,
        );
        assert!(
            reader_side.session.is_some(),
            "a dialed session carried no id"
        );
        assert_eq!(
            reader_side.session, writer_side.session,
            "the two ends of one session report different ids"
        );
        let (received, sent) = (
            reader_side.result.clone().map_err(anyhow::Error::msg)?,
            writer_side.result.clone().map_err(anyhow::Error::msg)?,
        );
        assert_eq!(received.entries_received, sent.entries_sent);
        ids.push(reader_side.session);
    }
    assert_ne!(ids.first(), ids.get(1), "two sessions carried one id");
    writer.router.shutdown().await?;
    reader.router.shutdown().await?;
    Ok(())
}
