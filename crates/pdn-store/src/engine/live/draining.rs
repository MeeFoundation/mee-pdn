//! The sync actor as the live actor reaches it. The sync actor waits for room
//! in the live actor's replica events channel ([`Delivery::Blocking`]), so
//! every wait of the live actor for its reply keeps taking those events: a
//! bare wait leaves both actors waiting for good once the sync actor has more
//! events to emit ahead of the reply than the channel holds. Specified in the
//! data layer's `change-subscription` spec.
//!
//! [`Delivery::Blocking`]: crate::subscribers::Delivery::Blocking

use std::{collections::VecDeque, future::Future, num::NonZeroU64};

use anyhow::Result;
use irpc::channel::mpsc;

use crate::{
    actor::{OpenOpts, SyncHandle},
    api::RpcResult,
    store::{DownloadPolicy, Query, Store},
    AuthorHeads, Event, NamespaceId, PeerIdBytes, SignedEntry,
};

/// Events the channel holds before the sync actor waits.
const EVENTS_CAP: usize = 1024;

pub(super) struct DrainingSync {
    sync: SyncHandle,
    events: Events,
}

struct Events {
    tx: async_channel::Sender<Event>,
    rx: async_channel::Receiver<Event>,
    /// Taken during a wait; they precede the events still in the channel.
    held: VecDeque<Event>,
}

impl Events {
    async fn during<T>(&mut self, reply: impl Future<Output = T>) -> T {
        let mut reply = std::pin::pin!(reply);
        loop {
            tokio::select! {
                biased;
                out = &mut reply => return out,
                event = self.rx.recv() => match event {
                    Ok(event) => self.held.push_back(event),
                    // The channel's sender lives beside its receiver.
                    Err(_closed) => return reply.await,
                },
            }
        }
    }
}

impl DrainingSync {
    pub(super) fn new(sync: SyncHandle) -> Self {
        let (tx, rx) = async_channel::bounded(EVENTS_CAP);
        Self {
            sync,
            events: Events {
                tx,
                rx,
                held: VecDeque::new(),
            },
        }
    }

    /// For a task off the live actor's loop: a wait on it inside the loop
    /// takes no events.
    pub(super) fn handle(&self) -> &SyncHandle {
        &self.sync
    }

    /// The next replica event, held ones first. Cancel-safe.
    pub(super) async fn next_event(&mut self) -> Result<Event, async_channel::RecvError> {
        match self.events.held.pop_front() {
            Some(event) => Ok(event),
            None => self.events.rx.recv().await,
        }
    }

    /// Waits for `reply` while taking replica events.
    pub(super) async fn wait<T>(&mut self, reply: impl Future<Output = T>) -> T {
        self.events.during(reply).await
    }

    /// Opens `namespace` for sync, its events delivered to the live actor.
    pub(super) async fn open(&mut self, namespace: NamespaceId) -> Result<()> {
        let opts = OpenOpts::default().sync().subscribe(self.events.tx.clone());
        self.events.during(self.sync.open(namespace, opts)).await
    }

    pub(super) async fn unsubscribe(&mut self, namespace: NamespaceId) -> Result<()> {
        let tx = self.events.tx.clone();
        self.events
            .during(self.sync.unsubscribe(namespace, tx))
            .await
    }

    pub(super) async fn set_sync(&mut self, namespace: NamespaceId, on: bool) -> Result<()> {
        self.events.during(self.sync.set_sync(namespace, on)).await
    }

    pub(super) async fn close(&mut self, namespace: NamespaceId) -> Result<bool> {
        self.events.during(self.sync.close(namespace)).await
    }

    pub(super) async fn get_sync_peers(
        &mut self,
        namespace: NamespaceId,
    ) -> Result<Option<Vec<PeerIdBytes>>> {
        self.events
            .during(self.sync.get_sync_peers(namespace))
            .await
    }

    pub(super) async fn register_useful_peer(
        &mut self,
        namespace: NamespaceId,
        peer: PeerIdBytes,
    ) -> Result<()> {
        self.events
            .during(self.sync.register_useful_peer(namespace, peer))
            .await
    }

    pub(super) async fn get_download_policy(
        &mut self,
        namespace: NamespaceId,
    ) -> Result<DownloadPolicy> {
        self.events
            .during(self.sync.get_download_policy(namespace))
            .await
    }

    /// The entries arrive on `reply`; wait on it through [`Self::wait`].
    pub(super) async fn get_many(
        &mut self,
        namespace: NamespaceId,
        query: Query,
        reply: mpsc::Sender<RpcResult<SignedEntry>>,
    ) -> Result<()> {
        self.events
            .during(self.sync.get_many(namespace, query, reply))
            .await
    }

    pub(super) async fn has_news_for_us(
        &mut self,
        namespace: NamespaceId,
        heads: AuthorHeads,
    ) -> Result<Option<NonZeroU64>> {
        self.events
            .during(self.sync.has_news_for_us(namespace, heads))
            .await
    }

    pub(super) async fn shutdown(&mut self) -> Result<Store> {
        self.events.during(self.sync.shutdown()).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::{bail, Context};
    use iroh_blobs::Hash;

    use super::*;
    use crate::{Author, NamespaceSecret};

    /// A reply the live actor waits for arrives while the sync actor waits
    /// for room in the live actor's full events channel, and every event
    /// reaches the live actor in insertion order. The blocked insert is
    /// queued ahead of the request, so the sync actor reaches the request
    /// only once the wait has taken an event.
    #[tokio::test]
    async fn a_reply_arrives_while_the_sync_actor_waits_for_room_for_its_events() -> Result<()> {
        let sync = SyncHandle::spawn(Store::memory(), None, None, None, "draining".into());
        let namespace = sync
            .import_namespace(NamespaceSecret::new(&mut rand::rng()).into())
            .await?;
        let author = sync.import_author(Author::new(&mut rand::rng())).await?;
        let mut draining = DrainingSync::new(sync.clone());
        draining.open(namespace).await?;
        let insert = |index: usize| {
            sync.insert_local(
                namespace,
                author,
                format!("k/{index}").into_bytes().into(),
                Hash::new(index.to_le_bytes()),
                1,
            )
        };
        for index in 0..EVENTS_CAP {
            insert(index).await?;
        }
        let mut blocked = std::pin::pin!(insert(EVENTS_CAP));
        assert!(
            n0_future::future::now_or_never(blocked.as_mut()).is_none(),
            "the insert past the channel's capacity must wait for room"
        );

        tokio::time::timeout(Duration::from_secs(10), draining.get_sync_peers(namespace))
            .await
            .context("the sync actor never reached the request")??;
        blocked.await?;
        for index in 0..=EVENTS_CAP {
            match draining.next_event().await? {
                Event::LocalInsert { entry, .. } => {
                    assert_eq!(entry.key(), format!("k/{index}").as_bytes());
                }
                other => bail!("expected the insert of k/{index}, got {other:?}"),
            }
        }
        draining.shutdown().await?;
        Ok(())
    }
}
