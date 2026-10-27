use std::collections::{hash_map, HashMap};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::EndpointId;
use iroh_gossip::{
    api::{Event, GossipReceiver, GossipSender, JoinOptions},
    net::Gossip,
};
use n0_future::{
    task::{AbortHandle, JoinSet},
    StreamExt,
};
use tokio::sync::mpsc;
use tracing::{debug, instrument, warn};

use super::live::{Op, ToLiveActor};
use crate::NamespaceId;

#[derive(Debug)]
struct ActiveState {
    sender: GossipSender,
    abort_handle: AbortHandle,
    generation: u64,
}

#[derive(Debug)]
pub(crate) struct GossipState {
    gossip: Gossip,
    to_live_actor: mpsc::Sender<ToLiveActor>,
    active: HashMap<NamespaceId, ActiveState>,
    active_tasks: JoinSet<(NamespaceId, u64, Result<()>)>,
    /// Names the next subscription, so an event its receive loop sent
    /// before a quit is told from one of a later join of the same topic.
    next_generation: u64,
}

impl GossipState {
    pub fn new(gossip: Gossip, to_live_actor: mpsc::Sender<ToLiveActor>) -> Self {
        Self {
            gossip,
            to_live_actor,
            active: Default::default(),
            active_tasks: Default::default(),
            next_generation: 0,
        }
    }

    pub async fn join(&mut self, namespace: NamespaceId, bootstrap: Vec<EndpointId>) -> Result<()> {
        match self.active.entry(namespace) {
            hash_map::Entry::Occupied(mut entry) => {
                if !bootstrap.is_empty() {
                    entry.get_mut().sender.join_peers(bootstrap).await?;
                }
            }
            hash_map::Entry::Vacant(entry) => {
                let sub = self
                    .gossip
                    .subscribe_with_opts(namespace.into(), JoinOptions::with_bootstrap(bootstrap))
                    .await?;

                let (sender, stream) = sub.split();
                let to_live_actor = self.to_live_actor.clone();
                let generation = self.next_generation;
                self.next_generation = generation.wrapping_add(1);
                let abort_handle = self.active_tasks.spawn(async move {
                    let res = receive_loop(namespace, generation, stream, to_live_actor).await;

                    (namespace, generation, res)
                });
                entry.insert(ActiveState {
                    sender,
                    abort_handle,
                    generation,
                });
            }
        }
        Ok(())
    }

    /// Whether `generation` names the subscription `namespace` holds now.
    pub(crate) fn is_current(&self, namespace: &NamespaceId, generation: u64) -> bool {
        self.active
            .get(namespace)
            .is_some_and(|state| state.generation == generation)
    }

    pub(crate) fn quit(&mut self, topic: &NamespaceId) {
        if let Some(state) = self.active.remove(topic) {
            state.abort_handle.abort();
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        for (_, state) in self.active.drain() {
            state.abort_handle.abort();
        }
        self.progress().await
    }

    pub async fn broadcast_neighbors(&mut self, namespace: &NamespaceId, message: Bytes) {
        if let Some(state) = self.active.get_mut(namespace) {
            state.sender.broadcast_neighbors(message).await.ok();
        }
    }

    pub(crate) fn max_message_size(&self) -> usize {
        self.gossip.max_message_size()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Waits for every receive loop to end.
    pub async fn progress(&mut self) -> Result<()> {
        while self.ended().await?.is_some() {}
        Ok(())
    }

    /// The topic whose current receive loop ended on its own, now out of
    /// the active set, pending while no loop is left: a topic outlives its
    /// loop when a runtime shutting down cancels it, and a ready answer to
    /// the live actor's `select!` there would spin it inside one poll.
    /// Returns an error if a loop panicked.
    ///
    /// ## Cancel safety
    ///
    /// This function is fully cancel-safe.
    pub(crate) async fn next_ended(&mut self) -> Result<NamespaceId> {
        match self.ended().await? {
            Some(namespace) => Ok(namespace),
            None => std::future::pending().await,
        }
    }

    /// [`Self::next_ended`], `None` once no loop is left.
    async fn ended(&mut self) -> Result<Option<NamespaceId>> {
        while let Some(res) = self.active_tasks.join_next().await {
            match res {
                Err(err) if err.is_cancelled() => continue,
                Err(err) => return Err(err).context("gossip receive loop panicked"),
                Ok((namespace, generation, res)) => {
                    if let Err(err) = res {
                        warn!(?err, ?namespace, "gossip receive loop failed")
                    }
                    if self.is_current(&namespace, generation) {
                        self.active.remove(&namespace);
                        return Ok(Some(namespace));
                    }
                }
            }
        }
        Ok(None)
    }
}

#[instrument("gossip-recv", skip_all, fields(namespace=%namespace.fmt_short()))]
async fn receive_loop(
    namespace: NamespaceId,
    generation: u64,
    mut recv: GossipReceiver,
    to_sync_actor: mpsc::Sender<ToLiveActor>,
) -> Result<()> {
    for peer in recv.neighbors() {
        to_sync_actor
            .send(ToLiveActor::NeighborUp {
                namespace,
                peer,
                generation,
            })
            .await?;
    }
    while let Some(event) = recv.try_next().await? {
        match event {
            Event::Lagged => {
                debug!("gossip loop lagged - dropping gossip event");
                continue;
            }
            Event::Received(msg) => {
                // Skipped, never `?`: an error here ends the loop and the
                // namespace leaves the active set, so one undecodable message
                // from any topic member would silence the topic.
                let op: Op = match postcard::from_bytes(&msg.content) {
                    Ok(op) => op,
                    Err(error) => {
                        debug!(
                            len = msg.content.len(),
                            %error,
                            "received invalid gossip message, ignoring"
                        );
                        continue;
                    }
                };
                match op {
                    Op::Put(_entry) => {
                        // Content never rides the topic (content-free
                        // swarm): entries flow only over reconciliation,
                        // which the session access provider gates per peer.
                        // An entry broadcast — from a peer on an older
                        // build, or a malicious one trying to inject past
                        // the filter — is dropped, never inserted. Own
                        // writes announce their author head instead
                        // (`Op::SyncReport` from `broadcast_local_head`);
                        // the receiver pulls.
                        debug!(peer = %msg.delivered_from.fmt_short(), namespace = %namespace.fmt_short(), "dropping entry received via gossip: content does not ride the topic");
                    }
                    Op::ContentReady(hash) => {
                        to_sync_actor
                            .send(ToLiveActor::NeighborContentReady {
                                namespace,
                                node: msg.delivered_from,
                                hash,
                            })
                            .await?;
                    }
                    Op::SyncReport(report) => {
                        to_sync_actor
                            .send(ToLiveActor::IncomingSyncReport {
                                from: msg.delivered_from,
                                report,
                            })
                            .await?;
                    }
                }
            }
            Event::NeighborUp(peer) => {
                to_sync_actor
                    .send(ToLiveActor::NeighborUp {
                        namespace,
                        peer,
                        generation,
                    })
                    .await?;
            }
            Event::NeighborDown(peer) => {
                to_sync_actor
                    .send(ToLiveActor::NeighborDown {
                        namespace,
                        peer,
                        generation,
                    })
                    .await?;
            }
            other => warn!(
                ?other,
                "ignoring a gossip event this engine does not handle"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use iroh::{endpoint::presets, Endpoint};

    use super::*;

    /// A topic whose receive loop was cancelled without a quit, as a runtime
    /// shutting down cancels it, leaves the next end pending rather than
    /// ready with nothing, and waiting for every loop still finishes.
    #[tokio::test]
    async fn a_topic_whose_loop_was_cancelled_leaves_the_next_end_pending() -> Result<()> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0u16))?
            .bind()
            .await?;
        let (to_live_actor, _inbox) = mpsc::channel(8);
        let mut state = GossipState::new(Gossip::builder().spawn(endpoint.clone()), to_live_actor);
        let namespace = NamespaceId::from(&[7; 32]);
        state.join(namespace, Vec::new()).await?;
        state
            .active
            .get(&namespace)
            .context("the topic was joined")?
            .abort_handle
            .abort();

        let ended = tokio::time::timeout(Duration::from_millis(200), state.next_ended()).await;
        assert!(
            ended.is_err(),
            "a cancelled loop made an end ready: {ended:?}"
        );
        assert!(!state.is_empty());
        tokio::time::timeout(Duration::from_secs(5), state.progress()).await??;

        endpoint.close().await;
        Ok(())
    }
}
