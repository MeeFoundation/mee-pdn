use std::future::Future;

use anyhow::{anyhow, ensure};
use bytes::{Buf, BufMut, BytesMut};
use iroh::PublicKey;
use n0_future::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};
use tracing::{debug, trace, Span};

use crate::{
    actor::SyncHandle,
    net::{AbortReason, AcceptError, AcceptOutcome, ConnectError},
    Identity, NamespaceId, SyncOutcome,
};

/// Frames longer than `ceiling` are refused on their length prefix, before
/// their body is read.
#[derive(Debug)]
pub(super) struct SyncCodec {
    ceiling: usize,
}

impl Default for SyncCodec {
    fn default() -> Self {
        Self {
            ceiling: MAX_MESSAGE_SIZE,
        }
    }
}

impl SyncCodec {
    /// The codec a session's first frame is read with, before anything
    /// classifies the caller.
    fn opening() -> Self {
        Self {
            ceiling: MAX_OPENING_FRAME,
        }
    }
}

const MAX_MESSAGE_SIZE: usize = 1024 * 1024 * 1024; // This is likely too large, but lets have some restrictions

/// The ceiling on a session's first frame, read before anything classifies
/// the caller: its two range boundaries carry one key each, at most
/// [`MAX_KEY_BYTES`](crate::sync::MAX_KEY_BYTES), and
/// `the_largest_honest_opening_fits_under_the_ceiling` holds the margin.
const MAX_OPENING_FRAME: usize = 32 * 1024;

impl Decoder for SyncCodec {
    type Item = Message;
    type Error = anyhow::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        let bytes: [u8; 4] = src[..4].try_into().unwrap();
        let frame_len = u32::from_be_bytes(bytes) as usize;
        ensure!(
            frame_len <= self.ceiling,
            "received message that is too large: {}",
            frame_len
        );
        if src.len() < 4 + frame_len {
            return Ok(None);
        }

        let message: Message = postcard::from_bytes(&src[4..4 + frame_len])?;
        src.advance(4 + frame_len);
        Ok(Some(message))
    }
}

impl Encoder<Message> for SyncCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len =
            postcard::serialize_with_flavor(&item, postcard::ser_flavors::Size::default()).unwrap();
        ensure!(
            len <= MAX_MESSAGE_SIZE,
            "attempting to send message that is too large {}",
            len
        );

        dst.put_u32(u32::try_from(len).expect("already checked"));
        if dst.len() < 4 + len {
            dst.resize(4 + len, 0u8);
        }
        postcard::to_slice(&item, &mut dst[4..])?;

        Ok(())
    }
}

/// Sync Protocol
///
/// - Init message: names the namespace, the identity whose replica is
///   addressed and the identity the caller acts for
/// - N Sync messages
///
/// On any error and on success the substream is closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum Message {
    /// Init message (sent by the dialing peer)
    Init(Init),
    /// Sync messages (sent by both peers)
    Sync(crate::sync::ProtocolMessage),
    /// Abort message (sent by the accepting peer to decline a request)
    Abort { reason: AbortReason },
}

/// A session's first message. The two identities are what an accepted
/// connection is dispatched by, so they are read before any replica is
/// touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Init {
    /// Namespace to sync
    pub(super) namespace: NamespaceId,
    /// Whose replica the session addresses.
    pub(super) identity: Identity,
    /// Whom the caller acts for.
    pub(super) caller: Identity,
    /// Initial message
    pub(super) message: crate::sync::ProtocolMessage,
}

/// Runs the initiator side of the sync protocol.
///
/// `identity` names whose replica this session addresses on the peer and
/// `caller` whom this side acts for; the peer dispatches the connection by
/// the first and judges the session by the second.
///
/// `filter` is this side's egress filter for the session: every value this
/// side reveals — the initial range boundary and fingerprint included —
/// derives from the filtered view.
///
/// The session reads through a store snapshot frozen here, before the
/// initial message: entries written after this point are not served
/// within this session (they travel on the next one). The snapshot is
/// released when this function exits, on every path.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_alice<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    writer: &mut W,
    reader: &mut R,
    handle: &SyncHandle,
    namespace: NamespaceId,
    identity: Identity,
    caller: Identity,
    peer: PublicKey,
    filter: Option<crate::filter::EntryFilter>,
    ingest: Option<crate::filter::SessionIngest>,
) -> Result<SyncOutcome, ConnectError> {
    let peer_bytes = *peer.as_bytes();
    let mut reader = FramedRead::new(reader, SyncCodec::default());
    let mut writer = FramedWrite::new(writer, SyncCodec::default());

    let mut progress = SyncOutcome::default();

    // Session setup: the guard holds the egress snapshot until drop.
    let session = handle
        .sync_session_start(namespace)
        .await
        .map_err(ConnectError::sync)?;

    // Init message

    let message = handle
        .sync_initial_message(namespace, session.id(), filter.clone())
        .await
        .map_err(ConnectError::sync)?;
    let init_message = Message::Init(Init {
        namespace,
        identity,
        caller,
        message,
    });
    trace!("send init message");
    writer
        .send(init_message)
        .await
        .map_err(ConnectError::sync)?;

    // Sync message loop
    while let Some(msg) = reader.next().await {
        let msg = msg.map_err(ConnectError::sync)?;
        match msg {
            Message::Init(_) => {
                return Err(ConnectError::sync(anyhow!("unexpected init message")));
            }
            Message::Sync(msg) => {
                trace!("recv process message");
                let current_progress = std::mem::take(&mut progress);
                let (reply, next_progress) = handle
                    .sync_process_message(
                        namespace,
                        msg,
                        peer_bytes,
                        current_progress,
                        session.id(),
                        filter.clone(),
                        ingest.clone(),
                    )
                    .await
                    .map_err(ConnectError::sync)?;
                progress = next_progress;
                if let Some(msg) = reply {
                    trace!("send process message");
                    writer
                        .send(Message::Sync(msg))
                        .await
                        .map_err(ConnectError::sync)?;
                } else {
                    break;
                }
            }
            Message::Abort { reason } => {
                return Err(ConnectError::remote_abort(reason));
            }
        }
    }

    trace!("done");
    Ok(progress)
}

/// A session read as far as its first message, before any replica is
/// touched: the point an accepted connection is dispatched from.
pub struct SessionOpening<R, W> {
    peer: PublicKey,
    init: Init,
    reader: FramedRead<R, SyncCodec>,
    writer: FramedWrite<W, SyncCodec>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> SessionOpening<R, W> {
    /// Read the session's first message off `reader`.
    pub(crate) async fn read(writer: W, reader: R, peer: PublicKey) -> Result<Self, AcceptError> {
        let mut reader = FramedRead::new(reader, SyncCodec::opening());
        let writer = FramedWrite::new(writer, SyncCodec::default());
        let opened = reader.next().await.ok_or_else(|| {
            AcceptError::sync(peer, None, anyhow!("stream closed before the init message"))
        })?;
        match opened.map_err(|e| AcceptError::sync(peer, None, e))? {
            Message::Init(init) => Ok(Self {
                peer,
                init,
                reader,
                writer,
            }),
            Message::Sync(_) | Message::Abort { .. } => Err(AcceptError::sync(
                peer,
                None,
                anyhow!("unexpected message before init"),
            )),
        }
    }

    /// Decline the session with a terminal frame, the way the rounds
    /// decline one: closing the stream alone reads to the initiator as an
    /// exchange that carried nothing.
    pub(super) async fn refuse(mut self, reason: AbortReason) -> Result<(W, R), AcceptError> {
        let sent = self.writer.send(Message::Abort { reason }).await;
        let (writer, reader) = (self.writer.into_inner(), self.reader.into_inner());
        match sent {
            Ok(()) => Ok((writer, reader)),
            Err(err) => Err(AcceptError::sync(self.peer, Some(self.init.namespace), err)),
        }
    }
}

impl<R, W> SessionOpening<R, W> {
    /// Whose replica this session addresses.
    pub fn identity(&self) -> Identity {
        self.init.identity
    }

    /// Whom the caller acts for.
    pub fn caller(&self) -> Identity {
        self.init.caller
    }

    /// The namespace the caller named.
    pub fn namespace(&self) -> NamespaceId {
        self.init.namespace
    }

    /// The caller's transport-authenticated node id.
    pub fn peer(&self) -> PublicKey {
        self.peer
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        PublicKey,
        Init,
        FramedRead<R, SyncCodec>,
        FramedWrite<W, SyncCodec>,
    ) {
        // Past the opening the caller is classified, and its frames read
        // under the session ceiling.
        let mut reader = self.reader;
        *reader.decoder_mut() = SyncCodec::default();
        (self.peer, self.init, reader, self.writer)
    }
}

impl<R, W> std::fmt::Debug for SessionOpening<R, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOpening")
            .field("peer", &self.peer.fmt_short().to_string())
            .field("namespace", &self.init.namespace.fmt_short().to_string())
            .field("identity", &self.init.identity)
            .field("caller", &self.init.caller)
            .finish()
    }
}

/// Runs the receiver side of the sync protocol.
#[cfg(test)]
pub(super) async fn run_bob<R, W, F, Fut>(
    writer: &mut W,
    reader: &mut R,
    handle: SyncHandle,
    accept_cb: F,
    peer: PublicKey,
) -> Result<(NamespaceId, SyncOutcome), AcceptError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(NamespaceId, Identity, Identity, PublicKey) -> Fut,
    Fut: Future<Output = AcceptOutcome>,
{
    let opening = SessionOpening::read(writer, reader, peer).await?;
    let (peer, init, mut reader, mut writer) = opening.into_parts();
    let mut state = BobState::new(peer);
    let namespace = state
        .run(&mut writer, &mut reader, init, handle, accept_cb)
        .await?;
    Ok((namespace, state.into_outcome()))
}

/// State for the receiver side of the sync protocol.
pub struct BobState {
    /// The namespace the peer named in its Init, set before the accept
    /// decision. Present without a session for the whole span from that
    /// point to the session opening, which is why the two are separate
    /// fields: every error in that span still has to name the namespace.
    namespace: Option<NamespaceId>,
    peer: PublicKey,
    /// What the rounds that completed accumulated, readable after a failed
    /// round too: a round hands the actor a copy and keeps this one, so a
    /// failure inside the actor loses that round's own counts and nothing
    /// else.
    progress: SyncOutcome,
    /// This side's egress filter for the session, taken from the accept
    /// decision and frozen for the session.
    filter: Option<crate::filter::EntryFilter>,
    /// This side's ingest verdict for the session, from the same decision
    /// and frozen the same way.
    ingest: Option<crate::filter::SessionIngest>,
    /// The session's egress snapshot, opened once the request is allowed
    /// and released when this state drops — on every session exit path.
    /// A session carries its own namespace, so the exchange rounds key on
    /// this alone and cannot read a session-less state as a syncing one.
    session: Option<crate::actor::SyncSession>,
}

impl BobState {
    /// Create a new state for a single connection.
    pub fn new(peer: PublicKey) -> Self {
        Self {
            peer,
            namespace: None,
            progress: Default::default(),
            filter: None,
            ingest: None,
            session: None,
        }
    }

    fn fail(&self, reason: impl Into<anyhow::Error>) -> AcceptError {
        AcceptError::sync(self.peer, self.namespace(), reason.into())
    }

    /// Handle a session whose first message is already read, and run to end.
    ///
    /// An exchange that ends badly says so with a terminal frame: closing
    /// the stream is what a finished one does, and the initiator cannot
    /// tell the two apart from the wire alone.
    pub async fn run<R, W, F, Fut>(
        &mut self,
        writer: &mut FramedWrite<W, SyncCodec>,
        reader: &mut FramedRead<R, SyncCodec>,
        init: Init,
        sync: SyncHandle,
        accept_cb: F,
    ) -> Result<NamespaceId, AcceptError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
        F: Fn(NamespaceId, Identity, Identity, PublicKey) -> Fut,
        Fut: Future<Output = AcceptOutcome>,
    {
        let res = self.run_rounds(writer, reader, init, sync, accept_cb).await;
        if let Err(ref err) = res {
            // Closing the stream is how a finished exchange ends, so an error
            // that only closes it reads to the initiator as an exchange that
            // carried nothing — and a caller waiting to catch up takes that
            // for having caught up. The refusal branch sends its own frame.
            if !matches!(err, AcceptError::Abort { .. }) {
                // Best effort: the peer being gone is the ordinary reason to
                // be on this path, and the write failing here must not
                // replace an error that names the namespace.
                let _ = writer
                    .send(Message::Abort {
                        reason: AbortReason::InternalServerError,
                    })
                    .await;
            }
        }
        res
    }

    /// The exchange itself: the accept decision on the first message, then
    /// rounds until either side is done.
    async fn run_rounds<R, W, F, Fut>(
        &mut self,
        writer: &mut FramedWrite<W, SyncCodec>,
        reader: &mut FramedRead<R, SyncCodec>,
        init: Init,
        sync: SyncHandle,
        accept_cb: F,
    ) -> Result<NamespaceId, AcceptError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
        F: Fn(NamespaceId, Identity, Identity, PublicKey) -> Fut,
        Fut: Future<Output = AcceptOutcome>,
    {
        let Init {
            namespace,
            identity,
            caller,
            message,
        } = init;
        Span::current().record("namespace", tracing::field::display(&namespace.fmt_short()));
        trace!("recv init message");
        // Named from the peer's Init, before the decision that can fail:
        // the caller registers this pair as exchanging when it allows the
        // request, so every error from here on has to name the namespace or
        // the caller is never told the exchange ended and holds the pair
        // until restart.
        self.namespace = Some(namespace);
        match accept_cb(namespace, identity, caller, self.peer).await {
            AcceptOutcome::Allow { filter, ingest } => {
                trace!("allow request");
                self.filter = filter;
                self.ingest = ingest;
            }
            AcceptOutcome::Reject(reason) => {
                debug!(?reason, "reject request");
                writer
                    .send(Message::Abort { reason })
                    .await
                    .map_err(|e| self.fail(e))?;
                return Err(AcceptError::Abort {
                    namespace,
                    peer: self.peer,
                    reason,
                });
            }
        }
        // Session setup: freeze the egress snapshot before the first
        // message is processed. A rejected request never opens one.
        let session = sync
            .sync_session_start(namespace)
            .await
            .map_err(|e| self.fail(e))?;
        let session_id = session.id();
        self.session = Some(session);
        let mut next = sync
            .sync_process_message(
                namespace,
                message,
                *self.peer.as_bytes(),
                self.progress.clone(),
                session_id,
                self.filter.clone(),
                self.ingest.clone(),
            )
            .await;

        loop {
            let (reply, progress) = next.map_err(|e| self.fail(e))?;
            self.progress = progress;
            match reply {
                Some(msg) => {
                    trace!("send process message");
                    writer
                        .send(Message::Sync(msg))
                        .await
                        .map_err(|e| self.fail(e))?;
                }
                None => break,
            }
            let Some(msg) = reader.next().await else {
                break;
            };
            match msg.map_err(|e| self.fail(e))? {
                Message::Sync(msg) => {
                    trace!("recv process message");
                    next = sync
                        .sync_process_message(
                            namespace,
                            msg,
                            *self.peer.as_bytes(),
                            self.progress.clone(),
                            session_id,
                            self.filter.clone(),
                            self.ingest.clone(),
                        )
                        .await;
                }
                Message::Init(_) => {
                    return Err(self.fail(anyhow!("double init message")));
                }
                Message::Abort { .. } => {
                    return Err(self.fail(anyhow!("unexpected sync abort message")));
                }
            }
        }

        trace!("done");

        self.namespace()
            .ok_or_else(|| self.fail(anyhow!("Stream closed before init message")))
    }

    /// Get the namespace that is synced, if available.
    pub fn namespace(&self) -> Option<NamespaceId> {
        self.namespace
    }

    /// Consume self and get the [`SyncOutcome`] this connection's completed
    /// rounds accumulated.
    pub fn into_outcome(self) -> SyncOutcome {
        self.progress
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use iroh::SecretKey;
    use iroh_blobs::Hash;
    use rand::{CryptoRng, RngExt, SeedableRng};
    use tracing_test::traced_test;

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        actor::OpenOpts,
        store::{self, Query, Store},
        AuthorId, NamespaceSecret,
    };

    /// One identity on both sides: these scenarios test the codec, not the
    /// dispatch the identities exist for.
    const TEST_HOLDER: Identity = Identity::from_bytes([0u8; 32]);
    /// A cache big enough that nothing in a scenario evicts.
    #[cfg(feature = "fs-store")]
    const TEST_CACHE_BYTES: usize = 16 * 1024 * 1024;

    /// An empty first message for `namespace`, under the one test identity.
    /// `super::`: this module shadows the wire `Message` with its own alias.
    fn init_frame(namespace: NamespaceId) -> super::Message {
        super::Message::Init(super::Init {
            namespace,
            identity: TEST_HOLDER,
            caller: TEST_HOLDER,
            message: crate::ranger::Message::from_parts(vec![]),
        })
    }

    #[tokio::test]
    async fn test_sync_simple() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer_id = SecretKey::from_bytes(&[2u8; 32]).public();

        let mut alice_store = store::Store::memory();
        // For now uses same author on both sides.
        let author = alice_store.new_author(&mut rng).unwrap();

        let namespace = NamespaceSecret::new(&mut rng);

        let mut alice_replica = alice_store.new_replica(namespace.clone()).unwrap();
        let alice_replica_id = alice_replica.id();
        alice_replica
            .hash_and_insert("hello bob", &author, "from alice")
            .await
            .unwrap();

        let mut bob_store = store::Store::memory();
        let mut bob_replica = bob_store.new_replica(namespace.clone()).unwrap();
        let bob_replica_id = bob_replica.id();
        bob_replica
            .hash_and_insert("hello alice", &author, "from bob")
            .await
            .unwrap();

        assert_eq!(
            bob_store
                .get_many(bob_replica_id, Query::all(),)
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            alice_store
                .get_many(alice_replica_id, Query::all())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );

        // close the replicas because now the async actor will take over
        alice_store.close_replica(alice_replica_id);
        bob_store.close_replica(bob_replica_id);

        let (alice, bob) = tokio::io::duplex(64);

        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice);
        let alice_handle = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        alice_handle
            .open(namespace.id(), OpenOpts::default().sync())
            .await?;
        let namespace_id = namespace.id();
        let alice_handle2 = alice_handle.clone();
        let alice_task = tokio::task::spawn(async move {
            run_alice(
                &mut alice_writer,
                &mut alice_reader,
                &alice_handle2,
                namespace_id,
                TEST_HOLDER,
                TEST_HOLDER,
                bob_peer_id,
                None,
                None,
            )
            .await
        });

        let (mut bob_reader, mut bob_writer) = tokio::io::split(bob);
        let bob_handle = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());
        bob_handle
            .open(namespace.id(), OpenOpts::default().sync())
            .await?;
        let bob_handle2 = bob_handle.clone();
        let bob_task = tokio::task::spawn(async move {
            run_bob(
                &mut bob_writer,
                &mut bob_reader,
                bob_handle2,
                |_namespace, _identity, _caller, _peer| {
                    std::future::ready(AcceptOutcome::Allow {
                        filter: None,
                        ingest: None,
                    })
                },
                alice_peer_id,
            )
            .await
        });

        alice_task.await??;
        bob_task.await??;

        let mut alice_store = alice_handle.shutdown().await?;
        let mut bob_store = bob_handle.shutdown().await?;

        assert_eq!(
            bob_store
                .get_many(namespace.id(), Query::all())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            alice_store
                .get_many(namespace.id(), Query::all())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            2
        );

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_sync_many_authors_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();
        test_sync_many_authors(alice_store, bob_store).await
    }

    #[tokio::test]
    #[traced_test]
    #[cfg(feature = "fs-store")]
    async fn test_sync_many_authors_fs() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let alice_store =
            store::fs::Store::persistent(tmpdir.path().join("a.db"), TEST_CACHE_BYTES)?;
        let bob_store = store::fs::Store::persistent(tmpdir.path().join("b.db"), TEST_CACHE_BYTES)?;
        test_sync_many_authors(alice_store, bob_store).await
    }

    type Message = (AuthorId, Vec<u8>, Hash);

    async fn insert_messages(
        mut rng: impl CryptoRng,
        replica: &mut crate::sync::Replica<'_>,
        num_authors: usize,
        msgs_per_author: usize,
        key_value_fn: impl Fn(&AuthorId, usize) -> (String, String),
    ) -> Vec<Message> {
        let mut res = vec![];
        let authors: Vec<_> = (0..num_authors)
            .map(|_| replica.store.store.new_author(&mut rng).unwrap())
            .collect();

        for i in 0..msgs_per_author {
            for author in authors.iter() {
                let (key, value) = key_value_fn(&author.id(), i);
                let hash = replica
                    .hash_and_insert(key.clone(), author, value)
                    .await
                    .unwrap();
                res.push((author.id(), key.as_bytes().to_vec(), hash));
            }
        }
        res.sort();
        res
    }

    fn get_messages(store: &mut Store, namespace: NamespaceId) -> Vec<Message> {
        let mut msgs = store
            .get_many(namespace, Query::all())
            .unwrap()
            .map(|entry| {
                entry.map(|entry| {
                    (
                        entry.author_bytes(),
                        entry.key().to_vec(),
                        entry.content_hash(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()
            .unwrap();
        msgs.sort();
        msgs
    }

    async fn test_sync_many_authors(mut alice_store: Store, mut bob_store: Store) -> Result<()> {
        let num_messages = &[1, 2, 5, 10];
        let num_authors = &[2, 3, 4, 5, 10];
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(99);

        for num_messages in num_messages {
            for num_authors in num_authors {
                println!(
                    "bob & alice each using {num_authors} authors and inserting {num_messages} messages per author"
                );

                let alice_node_pubkey = SecretKey::from_bytes(&rng.random()).public();
                let bob_node_pubkey = SecretKey::from_bytes(&rng.random()).public();
                let namespace = NamespaceSecret::new(&mut rng);

                let mut all_messages = vec![];

                let mut alice_replica = alice_store.new_replica(namespace.clone()).unwrap();
                let alice_messages = insert_messages(
                    &mut rng,
                    &mut alice_replica,
                    *num_authors,
                    *num_messages,
                    |author, i| {
                        (
                            format!("hello bob {i}"),
                            format!("from alice by {author}: {i}"),
                        )
                    },
                )
                .await;
                all_messages.extend_from_slice(&alice_messages);

                let mut bob_replica = bob_store.new_replica(namespace.clone()).unwrap();
                let bob_messages = insert_messages(
                    &mut rng,
                    &mut bob_replica,
                    *num_authors,
                    *num_messages,
                    |author, i| {
                        (
                            format!("hello bob {i}"),
                            format!("from bob by {author}: {i}"),
                        )
                    },
                )
                .await;
                all_messages.extend_from_slice(&bob_messages);

                all_messages.sort();

                let res = get_messages(&mut alice_store, namespace.id());
                assert_eq!(res, alice_messages);

                let res = get_messages(&mut bob_store, namespace.id());
                assert_eq!(res, bob_messages);

                // replicas can be opened only once so close the replicas before spawning the
                // actors
                alice_store.close_replica(namespace.id());
                let alice_handle =
                    SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());

                bob_store.close_replica(namespace.id());
                let bob_handle = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());

                run_sync(
                    alice_handle.clone(),
                    alice_node_pubkey,
                    bob_handle.clone(),
                    bob_node_pubkey,
                    namespace.id(),
                )
                .await?;

                alice_store = alice_handle.shutdown().await?;
                bob_store = bob_handle.shutdown().await?;

                let res = get_messages(&mut bob_store, namespace.id());
                assert_eq!(res.len(), all_messages.len());
                assert_eq!(res, all_messages);

                let res = get_messages(&mut bob_store, namespace.id());
                assert_eq!(res.len(), all_messages.len());
                assert_eq!(res, all_messages);
            }
        }

        Ok(())
    }

    async fn run_sync(
        alice_handle: SyncHandle,
        alice_node_pubkey: PublicKey,
        bob_handle: SyncHandle,
        bob_node_pubkey: PublicKey,
        namespace: NamespaceId,
    ) -> Result<()> {
        let (alice, bob) = run_sync_with_acceptor(
            alice_handle,
            alice_node_pubkey,
            bob_handle,
            bob_node_pubkey,
            namespace,
            |_namespace, _identity, _caller, _peer| {
                std::future::ready(AcceptOutcome::Allow {
                    filter: None,
                    ingest: None,
                })
            },
        )
        .await?;
        alice?;
        bob?;
        Ok(())
    }

    /// Both sides over one duplex, with the acceptor's decision left to the
    /// caller. Returns each side's own result, so a test can assert about a
    /// refusal as well as a success.
    async fn run_sync_with_acceptor<F, Fut>(
        alice_handle: SyncHandle,
        alice_node_pubkey: PublicKey,
        bob_handle: SyncHandle,
        bob_node_pubkey: PublicKey,
        namespace: NamespaceId,
        accept_cb: F,
    ) -> Result<(
        Result<SyncOutcome, ConnectError>,
        Result<(NamespaceId, SyncOutcome), AcceptError>,
    )>
    where
        F: Fn(NamespaceId, Identity, Identity, PublicKey) -> Fut + Send + 'static,
        Fut: Future<Output = AcceptOutcome> + Send,
    {
        alice_handle
            .open(namespace, OpenOpts::default().sync())
            .await?;
        bob_handle
            .open(namespace, OpenOpts::default().sync())
            .await?;
        let (alice, bob) = tokio::io::duplex(1024);

        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice);
        let alice_task = tokio::task::spawn(async move {
            run_alice(
                &mut alice_writer,
                &mut alice_reader,
                &alice_handle,
                namespace,
                TEST_HOLDER,
                TEST_HOLDER,
                bob_node_pubkey,
                None,
                None,
            )
            .await
        });

        let (mut bob_reader, mut bob_writer) = tokio::io::split(bob);
        let bob_task = tokio::task::spawn(async move {
            run_bob(
                &mut bob_writer,
                &mut bob_reader,
                bob_handle,
                accept_cb,
                alice_node_pubkey,
            )
            .await
        });

        Ok((alice_task.await?, bob_task.await?))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_sync_timestamps_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();
        test_sync_timestamps(alice_store, bob_store).await
    }

    #[tokio::test]
    #[traced_test]
    #[cfg(feature = "fs-store")]
    async fn test_sync_timestamps_fs() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let alice_store =
            store::fs::Store::persistent(tmpdir.path().join("a.db"), TEST_CACHE_BYTES)?;
        let bob_store = store::fs::Store::persistent(tmpdir.path().join("b.db"), TEST_CACHE_BYTES)?;
        test_sync_timestamps(alice_store, bob_store).await
    }

    async fn test_sync_timestamps(mut alice_store: Store, mut bob_store: Store) -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(99);
        let alice_node_pubkey = SecretKey::from_bytes(&rng.random()).public();
        let bob_node_pubkey = SecretKey::from_bytes(&rng.random()).public();
        let namespace = NamespaceSecret::new(&mut rng);

        let author = alice_store.new_author(&mut rng)?;
        bob_store.import_author(author.clone())?;

        let key = vec![1u8];
        let value_alice = vec![2u8];
        let value_bob = vec![3u8];
        let mut alice_replica = alice_store.new_replica(namespace.clone()).unwrap();
        let mut bob_replica = bob_store.new_replica(namespace.clone()).unwrap();
        // Insert into alice
        let hash_alice = alice_replica
            .hash_and_insert(&key, &author, &value_alice)
            .await
            .unwrap();
        // Insert into bob
        let hash_bob = bob_replica
            .hash_and_insert(&key, &author, &value_bob)
            .await
            .unwrap();

        assert_eq!(
            get_messages(&mut alice_store, namespace.id()),
            vec![(author.id(), key.clone(), hash_alice)]
        );

        assert_eq!(
            get_messages(&mut bob_store, namespace.id()),
            vec![(author.id(), key.clone(), hash_bob)]
        );

        alice_store.close_replica(namespace.id());
        bob_store.close_replica(namespace.id());

        let alice_handle = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        let bob_handle = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());

        run_sync(
            alice_handle.clone(),
            alice_node_pubkey,
            bob_handle.clone(),
            bob_node_pubkey,
            namespace.id(),
        )
        .await?;
        let mut alice_store = alice_handle.shutdown().await?;
        let mut bob_store = bob_handle.shutdown().await?;

        assert_eq!(
            get_messages(&mut alice_store, namespace.id()),
            vec![(author.id(), key.clone(), hash_bob)]
        );

        assert_eq!(
            get_messages(&mut bob_store, namespace.id()),
            vec![(author.id(), key.clone(), hash_bob)]
        );

        Ok(())
    }

    /// Spawns a handle over a store holding one open-for-sync replica.
    fn spawn_handle_with_replica(
        namespace: &NamespaceSecret,
        me: &str,
    ) -> Result<(SyncHandle, NamespaceId)> {
        let mut store = store::Store::memory();
        store.new_replica(namespace.clone())?;
        store.close_replica(namespace.id());
        let handle = SyncHandle::spawn(store, None, None, None, me.to_string());
        Ok((handle, namespace.id()))
    }

    /// A session carries both identities to the serving side, over a stream
    /// pair with no network under it.
    ///
    /// Denied: a caller that names a identity the serving side refuses sees
    /// its request declined and no entry served.
    #[tokio::test]
    async fn a_session_names_both_identities_to_the_serving_side() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();
        let addressed = Identity::from_bytes([7u8; 32]);
        let acting = Identity::from_bytes([9u8; 32]);

        let mut alice_store = store::Store::memory();
        let author = alice_store.new_author(&mut rng)?;
        let mut replica = alice_store.new_replica(namespace.clone())?;
        replica
            .hash_and_insert("greeting", &author, "hello")
            .await?;
        alice_store.close_replica(ns);
        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        let (bob, _) = spawn_handle_with_replica(&namespace, "bob")?;
        alice.open(ns, OpenOpts::default().sync()).await?;
        bob.open(ns, OpenOpts::default().sync()).await?;

        let seen: Arc<Mutex<Vec<(Identity, Identity)>>> = Arc::default();
        let (alice_io, bob_io) = tokio::io::duplex(1024);
        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice_io);
        let (bob_reader, bob_writer) = tokio::io::split(bob_io);

        let alice_run = alice.clone();
        let dial = tokio::task::spawn(async move {
            run_alice(
                &mut alice_writer,
                &mut alice_reader,
                &alice_run,
                ns,
                addressed,
                acting,
                bob_peer,
                None,
                None,
            )
            .await
        });

        let observed = Arc::clone(&seen);
        let serve = async move {
            let opening = SessionOpening::read(bob_writer, bob_reader, alice_peer).await?;
            assert_eq!(opening.identity(), addressed, "the addressed identity");
            assert_eq!(opening.caller(), acting, "the caller's identity");
            let (peer, init, mut reader, mut writer) = opening.into_parts();
            let mut state = BobState::new(peer);
            state
                .run(
                    &mut writer,
                    &mut reader,
                    init,
                    bob.clone(),
                    move |_ns, identity, caller, _peer| {
                        observed.lock().unwrap().push((identity, caller));
                        std::future::ready(AcceptOutcome::Allow {
                            filter: None,
                            ingest: None,
                        })
                    },
                )
                .await?;
            bob.shutdown().await?;
            anyhow::Ok(state.into_outcome())
        };

        let (dialed, served) = tokio::join!(dial, serve);
        dialed??;
        let served = served?;
        assert!(served.num_recv > 0, "the entry crossed the pipe");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[(addressed, acting)],
            "the accept decision saw the identities the caller named"
        );

        alice.shutdown().await?;
        Ok(())
    }

    /// A first message missing the caller's identity does not decode, so no
    /// session can be opened without one.
    #[test]
    fn an_init_without_the_caller_does_not_decode() {
        /// The first message as it would read without the second identity.
        #[derive(Serialize)]
        struct InitWithoutCaller {
            namespace: NamespaceId,
            identity: Identity,
            message: crate::sync::ProtocolMessage,
        }
        #[derive(Serialize)]
        enum Truncated {
            Init(InitWithoutCaller),
        }

        let namespace = NamespaceSecret::from_bytes(&[3u8; 32]).id();
        let bytes = postcard::to_stdvec(&Truncated::Init(InitWithoutCaller {
            namespace,
            identity: TEST_HOLDER,
            message: crate::ranger::Message::from_parts(vec![]),
        }))
        .expect("serialize");

        postcard::from_bytes::<super::Message>(&bytes)
            .expect_err("a first message without the caller's identity decoded");
    }

    /// Both sides of a completed sync exchange release their session
    /// snapshots.
    #[tokio::test]
    async fn test_sync_sessions_released_on_success() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer_id = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);

        let (alice_handle, namespace_id) = spawn_handle_with_replica(&namespace, "alice")?;
        let (bob_handle, _) = spawn_handle_with_replica(&namespace, "bob")?;
        let (alice, bob) = run_sync_with_acceptor(
            alice_handle.clone(),
            alice_peer_id,
            bob_handle.clone(),
            bob_peer_id,
            namespace_id,
            |_namespace, _identity, _caller, _peer| {
                std::future::ready(AcceptOutcome::Allow {
                    filter: None,
                    ingest: None,
                })
            },
        )
        .await?;
        alice?;
        bob?;

        // The guards dropped inside the runs; their release messages
        // precede these probes in the actor queues.
        assert_eq!(alice_handle.debug_session_count().await?, 0);
        assert_eq!(bob_handle.debug_session_count().await?, 0);

        alice_handle.shutdown().await?;
        bob_handle.shutdown().await?;
        Ok(())
    }

    /// A rejected request opens no session on the serving side, and the
    /// dialing side releases its snapshot on the error path.
    #[tokio::test]
    async fn test_sync_sessions_released_on_reject() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer_id = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);

        let (alice_handle, namespace_id) = spawn_handle_with_replica(&namespace, "alice")?;
        let (bob_handle, _) = spawn_handle_with_replica(&namespace, "bob")?;
        let (alice, bob) = run_sync_with_acceptor(
            alice_handle.clone(),
            alice_peer_id,
            bob_handle.clone(),
            bob_peer_id,
            namespace_id,
            |_namespace, _identity, _caller, _peer| {
                std::future::ready(AcceptOutcome::Reject(AbortReason::NotFound))
            },
        )
        .await?;
        assert!(alice.is_err());
        assert!(bob.is_err());
        assert_eq!(alice_handle.debug_session_count().await?, 0);
        assert_eq!(bob_handle.debug_session_count().await?, 0);

        alice_handle.shutdown().await?;
        bob_handle.shutdown().await?;
        Ok(())
    }

    /// A cancelled session run releases its snapshot: the guard drops with
    /// the future.
    #[tokio::test]
    async fn test_sync_session_released_on_cancelled_initiator() -> Result<()> {
        let mut rng = rand::rng();
        let bob_peer_id = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);

        let (alice_handle, namespace_id) = spawn_handle_with_replica(&namespace, "alice")?;
        alice_handle
            .open(namespace_id, OpenOpts::default().sync())
            .await?;

        // The peer end stays open and silent, so the run parks mid-session.
        let (alice, _bob_kept_silent) = tokio::io::duplex(64);
        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice);
        let alice_handle2 = alice_handle.clone();
        let alice_task = tokio::task::spawn(async move {
            run_alice(
                &mut alice_writer,
                &mut alice_reader,
                &alice_handle2,
                namespace_id,
                TEST_HOLDER,
                TEST_HOLDER,
                bob_peer_id,
                None,
                None,
            )
            .await
        });

        // Wait until the parked session's snapshot is registered.
        let mut registered = false;
        for _ in 0..1000 {
            if alice_handle.debug_session_count().await? == 1 {
                registered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registered, "session snapshot was never registered");

        alice_task.abort();
        assert!(alice_task.await.is_err());
        // The aborted future dropped the guard; its release message
        // precedes this probe in the actor queue.
        assert_eq!(alice_handle.debug_session_count().await?, 0);

        alice_handle.shutdown().await?;
        Ok(())
    }

    /// A round that fails leaves the accumulated outcome readable.
    /// `handle_connection` reads it on every path, before it looks at the
    /// result, so a state left unreadable here panics an accept task — and
    /// that panic leaves the actor driving every accepted sync, not just
    /// this connection.
    /// A session id names the actor that issued it, so one handle refuses
    /// another's id instead of resolving its own session of that number.
    ///
    /// Each actor counts its sessions from zero, so the first session of
    /// two handles carries the same number by construction — and both
    /// handles here hold the same namespace, which is the state the
    /// remaining checks (registered, right namespace) cannot tell apart.
    #[tokio::test]
    async fn a_session_id_is_refused_by_a_handle_that_did_not_issue_it() -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);

        let (issuer, namespace_id) = spawn_handle_with_replica(&namespace, "issuer")?;
        issuer
            .open(namespace_id, OpenOpts::default().sync())
            .await?;
        let (other, _) = spawn_handle_with_replica(&namespace, "other")?;
        other.open(namespace_id, OpenOpts::default().sync()).await?;

        let session = issuer.sync_session_start(namespace_id).await?;
        let other_session = other.sync_session_start(namespace_id).await?;
        assert_eq!(
            session.id(),
            session.id(),
            "an id is stable, so the comparison below is of actors"
        );
        assert_ne!(
            session.id(),
            other_session.id(),
            "two actors issued the same id, so nothing distinguishes them"
        );

        // Authorized: the issuing handle serves it.
        assert!(issuer
            .sync_initial_message(namespace_id, session.id(), None)
            .await
            .is_ok());
        // Refused: the other handle holds this namespace and a session of
        // its own, and still refuses an id it did not issue.
        assert!(other
            .sync_initial_message(namespace_id, session.id(), None)
            .await
            .is_err());

        drop(session);
        drop(other_session);
        issuer.shutdown().await?;
        other.shutdown().await?;
        Ok(())
    }

    /// An accept-side failure names the namespace once the request was
    /// allowed, and names nothing before that.
    ///
    /// The allow is where the caller registers the pair as exchanging, and
    /// it releases the pair on a result naming both peer and namespace. An
    /// error that names neither reads as a failure before the first
    /// message, so the pair is never released and stops syncing for the
    /// life of the process.
    #[tokio::test]
    async fn an_accept_failure_names_the_namespace_it_registered() -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut rng = rand::rng();
        let peer_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let (handle, namespace_id) = spawn_handle_with_replica(&namespace, "bob")?;
        // Open, but not for sync: the request is allowed and the session
        // then refuses — the failure between the two, which still has to
        // name the namespace.
        handle.open(namespace_id, OpenOpts::default()).await?;

        let allow = |_ns, _identity, _caller, _peer| {
            std::future::ready(AcceptOutcome::Allow {
                filter: None,
                ingest: None,
            })
        };

        let (bob_side, peer_side) = tokio::io::duplex(1024);
        let (bob_reader, bob_writer) = tokio::io::split(bob_side);
        let mut peer_writer = FramedWrite::new(peer_side, SyncCodec::default());
        peer_writer.send(init_frame(namespace_id)).await?;
        drop(peer_writer);

        let opening = SessionOpening::read(bob_writer, bob_reader, peer_id).await?;
        let (peer, init, mut bob_reader, mut bob_writer) = opening.into_parts();
        let mut state = BobState::new(peer);
        let err = state
            .run(
                &mut bob_writer,
                &mut bob_reader,
                init,
                handle.clone(),
                allow,
            )
            .await
            .expect_err("a replica open without sync served a session");
        assert_eq!(
            err.namespace(),
            Some(namespace_id),
            "the failure did not name the namespace the accept registered"
        );

        // The tightest case on the other side of the decision: a failure
        // before it names nothing, so no caller is told to release a pair
        // it never registered.
        let (bob_side, mut peer_side) = tokio::io::duplex(1024);
        let (bob_reader, bob_writer) = tokio::io::split(bob_side);
        peer_side
            .write_all(&[0, 0, 0, 4, 0xff, 0xff, 0xff, 0xff])
            .await?;
        drop(peer_side);

        let err = SessionOpening::read(bob_writer, bob_reader, peer_id)
            .await
            .expect_err("a malformed first message was accepted");
        assert_eq!(
            err.namespace(),
            None,
            "a failure before the accept named one"
        );

        handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_session_whose_handle_never_arrived_is_reclaimed() -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);
        let (handle, namespace_id) = spawn_handle_with_replica(&namespace, "node")?;
        handle
            .open(namespace_id, OpenOpts::default().sync())
            .await?;

        // A registration whose handle never reached its caller: the
        // cancellation a session timeout lands between registering the
        // snapshot and handing the handle back. The lost release message of
        // a completed session leaves the same state.
        handle.debug_abandon_session_start(namespace_id).await?;

        let mut reclaimed = false;
        for _ in 0..200 {
            if handle.debug_session_count().await? == 0 {
                reclaimed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(reclaimed, "the abandoned snapshot was never reclaimed");
        // The reclaim is counted, so the same event is visible on a running
        // node and not only from inside this test.
        assert_eq!(handle.metrics().sync_sessions_reclaimed.get(), 1);
        assert_eq!(handle.metrics().sync_sessions_open.get(), 0);

        // A session whose handle is alive is not swept out from under it.
        let session = handle.sync_session_start(namespace_id).await?;
        tokio::time::sleep(crate::actor::MAX_COMMIT_DELAY * 3).await;
        assert_eq!(handle.debug_session_count().await?, 1);
        assert_eq!(handle.metrics().sync_sessions_open.get(), 1);
        // Held, not reclaimed: the ordinary path leaves this counter alone.
        assert_eq!(handle.metrics().sync_sessions_reclaimed.get(), 1);
        assert!(handle
            .sync_initial_message(namespace_id, session.id(), None)
            .await
            .is_ok());
        drop(session);

        handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_round_leaves_the_outcome_readable() -> Result<()> {
        use crate::{
            ranger::{Fingerprint, MessagePart, RangeFingerprint},
            sync::RecordIdentifier,
            Author,
        };

        let mut rng = rand::rng();
        let peer_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let foreign = NamespaceSecret::new(&mut rng);
        let author = Author::new(&mut rng);

        let (handle, namespace_id) = spawn_handle_with_replica(&namespace, "bob")?;
        handle
            .open(namespace_id, OpenOpts::default().sync())
            .await?;

        // A boundary naming another namespace: the store refuses it, so the
        // round fails after the state was taken out for the call — the one
        // window in which a failure could leave the state unreadable.
        let range = crate::ranger::Range::new(
            RecordIdentifier::new(foreign.id(), author.id(), b""),
            RecordIdentifier::new(foreign.id(), author.id(), b"\xff"),
        );
        let refused = crate::ranger::Message::from_parts(vec![MessagePart::RangeFingerprint(
            RangeFingerprint {
                range,
                fingerprint: Fingerprint::empty(),
            },
        )]);

        let (bob_side, peer_side) = tokio::io::duplex(1024);
        let (bob_reader, bob_writer) = tokio::io::split(bob_side);
        // Sent up front and the peer end closed, so an unexpectedly served
        // round runs into end-of-stream instead of parking the test.
        let mut peer_writer = FramedWrite::new(peer_side, SyncCodec::default());
        peer_writer
            .send(super::Message::Init(super::Init {
                namespace: namespace_id,
                identity: TEST_HOLDER,
                caller: TEST_HOLDER,
                message: refused,
            }))
            .await?;
        drop(peer_writer);

        let opening = SessionOpening::read(bob_writer, bob_reader, peer_id).await?;
        let (peer, init, mut bob_reader, mut bob_writer) = opening.into_parts();
        let mut state = BobState::new(peer);
        let res = state
            .run(
                &mut bob_writer,
                &mut bob_reader,
                init,
                handle.clone(),
                |_ns, _identity, _caller, _peer| {
                    std::future::ready(AcceptOutcome::Allow {
                        filter: None,
                        ingest: None,
                    })
                },
            )
            .await;
        let err = res.expect_err("the refused range was served");
        assert!(
            format!("{err:?}").contains("another namespace"),
            "the round failed for another reason: {err:?}"
        );

        // The call `handle_connection` makes on every path.
        let outcome = state.into_outcome();
        assert_eq!(outcome.num_sent, 0);
        assert_eq!(outcome.num_recv, 0);

        handle.shutdown().await?;
        Ok(())
    }

    /// Session registry error paths and the replica-close sweep: a session
    /// requires an open, sync-enabled replica; a session id is bound to
    /// its namespace; closing the replica reclaims its sessions, and a
    /// stale id fails instead of silently reading live.
    #[tokio::test]
    async fn test_sync_session_lifecycle_errors_and_sweep() -> Result<()> {
        let mut rng = rand::rng();
        let ns1 = NamespaceSecret::new(&mut rng);
        let ns2 = NamespaceSecret::new(&mut rng);

        let mut store = store::Store::memory();
        store.new_replica(ns1.clone())?;
        store.close_replica(ns1.id());
        store.new_replica(ns2.clone())?;
        store.close_replica(ns2.id());
        let handle = SyncHandle::spawn(store, None, None, None, "node".to_string());

        // Not open: no session.
        assert!(handle.sync_session_start(ns1.id()).await.is_err());

        // Open without sync: no session.
        handle.open(ns1.id(), OpenOpts::default()).await?;
        assert!(handle.sync_session_start(ns1.id()).await.is_err());
        handle.close(ns1.id()).await?;

        // Open for sync: session opens and registers.
        handle.open(ns1.id(), OpenOpts::default().sync()).await?;
        handle.open(ns2.id(), OpenOpts::default().sync()).await?;
        let session = handle.sync_session_start(ns1.id()).await?;
        assert_eq!(handle.debug_session_count().await?, 1);

        // A session id is bound to its namespace.
        assert!(handle
            .sync_initial_message(ns2.id(), session.id(), None)
            .await
            .is_err());
        // The bound namespace serves through the snapshot.
        assert!(handle
            .sync_initial_message(ns1.id(), session.id(), None)
            .await
            .is_ok());

        // Closing the replica sweeps its sessions.
        handle.close(ns1.id()).await?;
        assert_eq!(handle.debug_session_count().await?, 0);

        // A stale id fails rather than resolving to whatever now sits at
        // its number. Reading without a session is not expressible: the id
        // is a required argument, so no caller can fall back to live reads
        // by omission.
        handle.open(ns1.id(), OpenOpts::default().sync()).await?;
        assert!(handle
            .sync_initial_message(ns1.id(), session.id(), None)
            .await
            .is_err());

        // The guard's late release of the swept session is a no-op.
        drop(session);
        assert_eq!(handle.debug_session_count().await?, 0);

        handle.shutdown().await?;
        Ok(())
    }

    /// A frame whose range boundary is shorter than a namespace and an
    /// author is refused here, at the decoder.
    ///
    /// The readers below slice both unchecked, and they run on the thread
    /// that owns the store: a boundary that reaches them takes down sync for
    /// every namespace of every identity the node hosts, on one frame from
    /// any peer holding a ticket to any one replica.
    #[test]
    fn a_frame_naming_a_short_range_boundary_is_refused() -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);
        let mut store = store::Store::memory();
        let mut replica = store.new_replica(namespace.clone())?;
        let initial = replica.sync_initial_message(None)?;
        drop(replica);

        let mut codec = SyncCodec::default();
        let mut frame = BytesMut::new();
        codec.encode(super::Message::Sync(initial), &mut frame)?;

        // Control: the frame decodes as it stands, so the splice below is
        // what the refusal is about.
        let mut intact = frame.clone();
        assert!(codec.decode(&mut intact)?.is_some());

        // Both boundaries of an empty replica are the default identifier: a
        // zeroed head behind its postcard length. Shrink the first to an
        // empty byte string, the shape a peer crafts.
        let head = crate::sync::ID_HEAD_BYTES;
        let boundary: Vec<u8> = std::iter::once(u8::try_from(head).unwrap())
            .chain(std::iter::repeat_n(0u8, head))
            .collect();
        let at = frame
            .windows(boundary.len())
            .position(|window| window == boundary)
            .expect("the initial message names the default identifier");
        let mut spliced = BytesMut::new();
        spliced.extend_from_slice(&frame[..at]);
        spliced.extend_from_slice(&[0u8]);
        spliced.extend_from_slice(&frame[at + boundary.len()..]);
        let body = u32::try_from(spliced.len() - 4).unwrap();
        spliced[..4].copy_from_slice(&body.to_be_bytes());

        assert!(codec.decode(&mut spliced).is_err());
        Ok(())
    }

    /// Runs one exchange to completion over the two handles, the way the
    /// wire loops do: each side's rounds go through its own session.
    async fn exchange_over_sessions(
        alice: &SyncHandle,
        alice_session: &crate::actor::SyncSession,
        alice_peer: PublicKey,
        bob: &SyncHandle,
        bob_session: &crate::actor::SyncSession,
        bob_peer: PublicKey,
        namespace: NamespaceId,
    ) -> Result<(SyncOutcome, SyncOutcome, usize)> {
        let mut alice_state = SyncOutcome::default();
        let mut bob_state = SyncOutcome::default();
        let mut messages = 0;
        let mut message = alice
            .sync_initial_message(namespace, alice_session.id(), None)
            .await?;
        loop {
            messages += 1;
            let (reply, next) = bob
                .sync_process_message(
                    namespace,
                    message,
                    *alice_peer.as_bytes(),
                    std::mem::take(&mut bob_state),
                    bob_session.id(),
                    None,
                    None,
                )
                .await?;
            bob_state = next;
            let Some(reply) = reply else { break };
            messages += 1;
            let (back, next) = alice
                .sync_process_message(
                    namespace,
                    reply,
                    *bob_peer.as_bytes(),
                    std::mem::take(&mut alice_state),
                    alice_session.id(),
                    None,
                    None,
                )
                .await?;
            alice_state = next;
            let Some(back) = back else { break };
            message = back;
        }
        Ok((alice_state, bob_state, messages))
    }

    /// A session serves the set frozen at its start, through the actor —
    /// which is where the snapshot is looked up and handed to the replica.
    ///
    /// Each stage writes between the session's start and its first message,
    /// and the write travels on the next session rather than this one.
    /// Reading live instead shows up in two ways, one per call the actor
    /// wires: the rounds hand the entry to the peer, and the initial message
    /// names a set the peer lacks, so two sides that hold the same entries
    /// start exchanging over nothing — which is what the last stage is for.
    #[tokio::test]
    async fn a_session_serves_the_view_frozen_at_its_start() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();

        let mut alice_store = store::Store::memory();
        alice_store.new_replica(namespace.clone())?;
        alice_store.close_replica(ns);
        let author = alice_store.new_author(&mut rng)?.id();
        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        let (bob, _) = spawn_handle_with_replica(&namespace, "bob")?;
        alice.open(ns, OpenOpts::default().sync()).await?;
        bob.open(ns, OpenOpts::default().sync()).await?;

        let write = async |key: &str| {
            alice
                .insert_local(
                    ns,
                    author,
                    key.as_bytes().to_vec().into(),
                    Hash::new(key),
                    key.len() as u64,
                )
                .await
        };
        for key in ["ape", "bee", "cat"] {
            write(key).await?;
        }
        let held = |handle: &SyncHandle, key: &'static str| {
            let handle = handle.clone();
            async move {
                anyhow::Ok(
                    handle
                        .get_exact(ns, author, key.as_bytes().to_vec().into(), false)
                        .await?
                        .is_some(),
                )
            }
        };

        // Session setup, then a write: the actor serves the snapshot it
        // registered, on the initial message and on every round after it.
        let alice_session = alice.sync_session_start(ns).await?;
        let bob_session = bob.sync_session_start(ns).await?;
        write("dog").await?;

        let (alice_state, bob_state, _) = exchange_over_sessions(
            &alice,
            &alice_session,
            alice_peer,
            &bob,
            &bob_session,
            bob_peer,
            ns,
        )
        .await?;

        // The mid-session write was not even transmitted.
        assert_eq!(alice_state.num_sent, 3);
        assert_eq!(bob_state.num_recv, 3);
        for key in ["ape", "bee", "cat"] {
            assert!(held(&bob, key).await?, "{key} did not reach the peer");
        }
        assert!(
            !held(&bob, "dog").await?,
            "the mid-session write was served"
        );
        assert!(held(&alice, "dog").await?, "the write is held at home");

        // The next session, on a fresh snapshot, carries it.
        drop((alice_session, bob_session));
        let alice_session = alice.sync_session_start(ns).await?;
        let bob_session = bob.sync_session_start(ns).await?;
        exchange_over_sessions(
            &alice,
            &alice_session,
            alice_peer,
            &bob,
            &bob_session,
            bob_peer,
            ns,
        )
        .await?;
        assert!(held(&bob, "dog").await?, "the next session withheld it too");

        // Both sides now hold the same set, which is the state the initial
        // message speaks about: its fingerprint decides whether there is
        // anything to exchange at all. Written to after its session starts,
        // this side still reports itself identical to the peer, and the
        // exchange ends on that one message.
        drop((alice_session, bob_session));
        let alice_session = alice.sync_session_start(ns).await?;
        let bob_session = bob.sync_session_start(ns).await?;
        write("eel").await?;
        let (alice_state, bob_state, messages) = exchange_over_sessions(
            &alice,
            &alice_session,
            alice_peer,
            &bob,
            &bob_session,
            bob_peer,
            ns,
        )
        .await?;
        assert_eq!(messages, 1, "the peers disagreed about being in sync");
        assert_eq!((alice_state.num_sent, alice_state.num_recv), (0, 0));
        assert_eq!((bob_state.num_sent, bob_state.num_recv), (0, 0));
        assert!(
            !held(&bob, "eel").await?,
            "the mid-session write was served"
        );

        drop((alice_session, bob_session));
        alice.shutdown().await?;
        bob.shutdown().await?;
        Ok(())
    }

    /// A serving side that fails after allowing the request reaches the
    /// initiator as a failed exchange, not as one that carried nothing.
    ///
    /// Closing the stream is what a finished exchange does, so without a
    /// terminal frame the two are the same on the wire — and a caller
    /// waiting to catch up reads the second as having caught up.
    #[tokio::test]
    async fn an_accept_side_failure_reaches_the_initiator_as_a_failure() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();

        let (alice, _) = spawn_handle_with_replica(&namespace, "alice")?;
        let (bob, _) = spawn_handle_with_replica(&namespace, "bob")?;
        alice.open(ns, OpenOpts::default().sync()).await?;
        // Open, but not for sync: the request is allowed and the session
        // then refuses — the race a newcomer hits when it dials an inviter
        // that has not enabled sync yet.
        bob.open(ns, OpenOpts::default()).await?;

        let (alice_io, bob_io) = tokio::io::duplex(1024);
        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice_io);
        let alice_run = alice.clone();
        let alice_task = tokio::task::spawn(async move {
            run_alice(
                &mut alice_writer,
                &mut alice_reader,
                &alice_run,
                ns,
                TEST_HOLDER,
                TEST_HOLDER,
                bob_peer,
                None,
                None,
            )
            .await
        });
        let (mut bob_reader, mut bob_writer) = tokio::io::split(bob_io);
        let bob_run = bob.clone();
        let bob_task = tokio::task::spawn(async move {
            run_bob(
                &mut bob_writer,
                &mut bob_reader,
                bob_run,
                |_namespace, _identity, _caller, _peer| {
                    std::future::ready(AcceptOutcome::Allow {
                        filter: None,
                        ingest: None,
                    })
                },
                alice_peer,
            )
            .await
        });

        let bob_res = bob_task.await?;
        let alice_res = alice_task.await?;
        assert!(bob_res.is_err(), "the serving side was expected to fail");
        assert!(
            alice_res.is_err(),
            "the initiator recorded a failed exchange as a successful one"
        );

        alice.shutdown().await?;
        bob.shutdown().await?;
        Ok(())
    }

    /// The outcome a failed exchange leaves behind is what its completed
    /// rounds accumulated, not zero.
    ///
    /// A round hands its counts to the actor, so a round that fails there
    /// must not take the earlier ones with it: the first thing anyone reads
    /// on this path is how much a cut exchange had moved.
    #[tokio::test]
    async fn a_failed_round_keeps_what_completed_rounds_accumulated() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();

        // Bob serves three entries and alice holds none, so bob's first
        // round is the one that moves them.
        let mut bob_store = store::Store::memory();
        bob_store.new_replica(namespace.clone())?;
        bob_store.close_replica(ns);
        let author = bob_store.new_author(&mut rng)?.id();
        let bob = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());
        bob.open(ns, OpenOpts::default().sync()).await?;
        for key in ["ape", "bee", "cat"] {
            bob.insert_local(
                ns,
                author,
                key.as_bytes().to_vec().into(),
                Hash::new(key),
                key.len() as u64,
            )
            .await?;
        }
        // Alice holds three of her own, so she answers bob's first round
        // instead of ending the exchange there.
        let mut alice_store = store::Store::memory();
        alice_store.new_replica(namespace.clone())?;
        alice_store.close_replica(ns);
        let alice_author = alice_store.new_author(&mut rng)?.id();
        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        alice.open(ns, OpenOpts::default().sync()).await?;
        for key in ["dog", "eel", "fox"] {
            alice
                .insert_local(
                    ns,
                    alice_author,
                    key.as_bytes().to_vec().into(),
                    Hash::new(key),
                    key.len() as u64,
                )
                .await?;
        }

        let (alice_io, bob_io) = tokio::io::duplex(1024);
        let (bob_reader, bob_writer) = tokio::io::split(bob_io);
        let bob_run = bob.clone();
        let (alice_reader, alice_writer) = tokio::io::split(alice_io);

        let mut state = BobState::new(alice_peer);
        let serve = async {
            let opening = SessionOpening::read(bob_writer, bob_reader, alice_peer).await?;
            let (_peer, init, mut reader, mut writer) = opening.into_parts();
            state
                .run(
                    &mut writer,
                    &mut reader,
                    init,
                    bob_run,
                    |_namespace, _identity, _caller, _peer| {
                        std::future::ready(AcceptOutcome::Allow {
                            filter: None,
                            ingest: None,
                        })
                    },
                )
                .await
        };

        let drive_alice = async {
            let mut reader = FramedRead::new(alice_reader, SyncCodec::default());
            let mut writer = FramedWrite::new(alice_writer, SyncCodec::default());
            let session = alice.sync_session_start(ns).await?;
            let message = alice.sync_initial_message(ns, session.id(), None).await?;
            writer
                .send(super::Message::Init(super::Init {
                    namespace: ns,
                    identity: TEST_HOLDER,
                    caller: TEST_HOLDER,
                    message,
                }))
                .await?;

            let reply = reader
                .next()
                .await
                .transpose()?
                .ok_or_else(|| anyhow!("the serving side answered nothing"))?;
            let super::Message::Sync(reply) = reply else {
                anyhow::bail!("expected a sync message, got {reply:?}");
            };

            // The replica goes away under the live session, so the next
            // round fails inside the actor — which is where a round's own
            // counts live while it runs.
            bob.close(ns).await?;

            let (next, _) = alice
                .sync_process_message(
                    ns,
                    reply,
                    *alice_peer.as_bytes(),
                    SyncOutcome::default(),
                    session.id(),
                    None,
                    None,
                )
                .await?;
            let next = next.ok_or_else(|| anyhow!("the exchange ended in one round"))?;
            writer.send(super::Message::Sync(next)).await?;
            anyhow::Ok(())
        };

        let (bob_res, alice_res) = tokio::join!(serve, drive_alice);
        alice_res?;
        assert!(bob_res.is_err(), "the round was expected to fail");
        let outcome = state.into_outcome();
        // How many the first round moves is a property of how the
        // reconciliation splits the range; that the count is not zero after
        // the second round failed is the property under test.
        assert!(
            outcome.num_sent > 0,
            "the completed round's counts went down with the failed round"
        );

        alice.shutdown().await?;
        bob.shutdown().await?;
        Ok(())
    }

    /// A refusal that cannot be written still names the namespace.
    ///
    /// The caller registers the pair as exchanging when it allows the
    /// request, and releases it by the namespace the error names. An error
    /// without one is routed as a failure before the first message, and the
    /// pair stays registered — no sync with that peer over that namespace
    /// until the process restarts.
    #[tokio::test]
    async fn a_refusal_that_cannot_be_written_still_names_the_namespace() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();

        let (alice, _) = spawn_handle_with_replica(&namespace, "alice")?;
        let (bob, _) = spawn_handle_with_replica(&namespace, "bob")?;
        alice.open(ns, OpenOpts::default().sync()).await?;
        bob.open(ns, OpenOpts::default().sync()).await?;

        let (alice_io, bob_io) = tokio::io::duplex(1024);
        let (mut bob_reader, mut bob_writer) = tokio::io::split(bob_io);

        let session = alice.sync_session_start(ns).await?;
        let message = alice.sync_initial_message(ns, session.id(), None).await?;
        let mut alice_framed = FramedWrite::new(alice_io, SyncCodec::default());
        alice_framed
            .send(super::Message::Init(super::Init {
                namespace: ns,
                identity: TEST_HOLDER,
                caller: TEST_HOLDER,
                message,
            }))
            .await?;

        // Dropping alice's end inside the decision puts her vanishing
        // exactly between the peer naming the namespace and the write of
        // the refusal, which is the window the naming has to survive.
        let alice_end = std::sync::Arc::new(std::sync::Mutex::new(Some(alice_framed)));
        let accept_cb = {
            let alice_end = std::sync::Arc::clone(&alice_end);
            move |_namespace, _identity, _caller, _peer| {
                alice_end.lock().unwrap().take();
                std::future::ready(AcceptOutcome::Reject(AbortReason::NotFound))
            }
        };

        let err = run_bob(
            &mut bob_writer,
            &mut bob_reader,
            bob.clone(),
            accept_cb,
            alice_peer,
        )
        .await
        .expect_err("the refusal could not be written");
        assert_eq!(
            err.namespace(),
            Some(ns),
            "the error names no namespace, so the pair is never released"
        );

        drop(session);
        alice.shutdown().await?;
        bob.shutdown().await?;
        Ok(())
    }

    /// A retraction that lands inside a session is served for the rest of
    /// it, and no later session takes it back.
    ///
    /// A removal replicates as absence, so the next session carries no news
    /// of it: the peer keeps what it was handed and offers it back. What
    /// removes it at the peer is the marker the retraction records above
    /// this layer, which arms the peer to refuse the entry — reconciliation
    /// alone converges on what a set gains, not on what it loses.
    #[tokio::test]
    async fn a_retraction_inside_a_session_is_served_for_the_rest_of_it() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);
        let ns = namespace.id();

        let mut alice_store = store::Store::memory();
        alice_store.new_replica(namespace.clone())?;
        alice_store.close_replica(ns);
        let author = alice_store.new_author(&mut rng)?.id();
        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        let (bob, _) = spawn_handle_with_replica(&namespace, "bob")?;
        alice.open(ns, OpenOpts::default().sync()).await?;
        bob.open(ns, OpenOpts::default().sync()).await?;
        for key in ["ape", "bee", "cat"] {
            alice
                .insert_local(
                    ns,
                    author,
                    key.as_bytes().to_vec().into(),
                    Hash::new(key),
                    key.len() as u64,
                )
                .await?;
        }
        let held = |handle: &SyncHandle, key: &'static str| {
            let handle = handle.clone();
            async move {
                anyhow::Ok(
                    handle
                        .get_exact(ns, author, key.as_bytes().to_vec().into(), false)
                        .await?
                        .is_some(),
                )
            }
        };

        // The session's view is taken here; the retraction lands after it.
        let alice_session = alice.sync_session_start(ns).await?;
        let bob_session = bob.sync_session_start(ns).await?;
        assert!(
            alice
                .retract_entry(ns, author, b"bee".to_vec().into(), u64::MAX)
                .await?,
            "the entry was not retracted"
        );
        assert!(
            !held(&alice, "bee").await?,
            "the row is gone from the store"
        );

        exchange_over_sessions(
            &alice,
            &alice_session,
            alice_peer,
            &bob,
            &bob_session,
            bob_peer,
            ns,
        )
        .await?;
        assert!(
            held(&bob, "bee").await?,
            "the frozen view was expected to serve the retracted entry"
        );

        // The next session carries no news of the removal — only the
        // absence of what was removed, which the peer reads as a set this
        // side is behind on.
        drop((alice_session, bob_session));
        let alice_session = alice.sync_session_start(ns).await?;
        let bob_session = bob.sync_session_start(ns).await?;
        exchange_over_sessions(
            &alice,
            &alice_session,
            alice_peer,
            &bob,
            &bob_session,
            bob_peer,
            ns,
        )
        .await?;
        assert!(held(&bob, "bee").await?, "the peer still holds it");
        assert!(
            held(&alice, "bee").await?,
            "and hands it back, which is what the marker above this layer refuses"
        );

        drop((alice_session, bob_session));
        alice.shutdown().await?;
        bob.shutdown().await?;
        Ok(())
    }

    /// A session released the ordinary way is never counted as reclaimed,
    /// even when the actor's queue falls behind.
    ///
    /// The reclaim pass goes by the strong count and runs before the action
    /// of its tick, so a release message still queued would read as a
    /// registration whose handle is gone — and the counter that is supposed
    /// to separate a lost release from an ordinary one would count both.
    ///
    /// Admitted instrument: the actor is parked by a subscriber that stops
    /// draining its bounded channel, because a release has to sit in the
    /// queue across a reclaim pass and nothing else in the API holds the
    /// actor still for that long.
    #[tokio::test]
    async fn an_ordinary_release_is_not_counted_as_reclaimed() -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);
        let mut store = store::Store::memory();
        store.new_replica(namespace.clone())?;
        store.close_replica(namespace.id());
        let author = store.new_author(&mut rng)?.id();
        let handle = SyncHandle::spawn(store, None, None, None, "node".to_string());
        let ns = namespace.id();
        handle.open(ns, OpenOpts::default().sync()).await?;

        let (events_tx, events_rx) = async_channel::bounded(1);
        handle.subscribe(ns, events_tx).await?;

        let write = |key: &'static str| {
            let handle = handle.clone();
            async move {
                handle
                    .insert_local(
                        ns,
                        author,
                        key.as_bytes().to_vec().into(),
                        Hash::new(key),
                        key.len() as u64,
                    )
                    .await
            }
        };

        let session = handle.sync_session_start(ns).await?;
        assert_eq!(handle.debug_session_count().await?, 1);

        // The first event fills the subscriber's channel; the second parks
        // the actor inside the action that emits it.
        write("ape").await?;
        let parked = tokio::task::spawn(write("bee"));
        // Polled rather than slept on: the actor is parked once it stops
        // answering, and the probe that goes unanswered stays in the queue,
        // which is what puts a tick between the unparking and the release.
        let mut is_parked = false;
        for _ in 0..50 {
            let probe = handle.clone();
            if tokio::time::timeout(std::time::Duration::from_millis(100), async move {
                probe.debug_session_count().await
            })
            .await
            .is_err()
            {
                is_parked = true;
                break;
            }
        }
        assert!(
            is_parked,
            "the actor was expected to park on the subscriber that stopped draining"
        );

        // Released the ordinary way, into a queue the actor is not reading.
        drop(session);

        // Long enough that the next tick's reclaim pass is due rather than
        // held off by its own rate limit.
        tokio::time::sleep(crate::actor::MAX_COMMIT_DELAY * 3).await;

        // Draining lets the actor go: it finishes the parked action, ticks
        // for the probe above — reclaiming as it starts that tick — and only
        // then reads the release.
        while events_rx.try_recv().is_ok() {}
        parked.await??;
        assert_eq!(handle.debug_session_count().await?, 0);
        assert_eq!(
            handle.metrics().sync_sessions_reclaimed.get(),
            0,
            "an ordinary release was counted as a lost one"
        );

        handle.shutdown().await?;
        Ok(())
    }

    /// An entry whose key is longer than `MAX_KEY_BYTES` is dropped at
    /// ingest, and the session goes on to deliver the rest. The long key is
    /// written past validation, as a modified node's store holds it.
    #[tokio::test]
    async fn an_entry_with_a_key_over_the_bound_is_dropped_at_ingest() -> Result<()> {
        use crate::ranger::Store as _;

        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);

        let mut alice_store = store::Store::memory();
        let author = alice_store.new_author(&mut rng)?;
        drop(alice_store.new_replica(namespace.clone())?);
        alice_store.close_replica(namespace.id());
        let short = b"short".to_vec();
        let long = vec![b'k'; crate::sync::MAX_KEY_BYTES + 1];
        {
            let mut raw = crate::store::fs::StoreInstance::new(namespace.id(), &mut alice_store);
            for key in [&short, &long] {
                raw.entry_put(crate::SignedEntry::from_parts(
                    &namespace,
                    &author,
                    key,
                    crate::Record::current_from_data("v"),
                ))?;
            }
        }
        let mut bob_store = store::Store::memory();
        drop(bob_store.new_replica(namespace.clone())?);
        bob_store.close_replica(namespace.id());

        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        alice
            .open(namespace.id(), OpenOpts::default().sync())
            .await?;
        let bob = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());
        bob.open(namespace.id(), OpenOpts::default().sync()).await?;
        run_sync(
            alice.clone(),
            alice_peer,
            bob.clone(),
            bob_peer,
            namespace.id(),
        )
        .await?;
        let _alice_store = alice.shutdown().await?;
        let mut bob_store = bob.shutdown().await?;

        let held = bob_store
            .get_many(namespace.id(), Query::all())?
            .map(|entry| entry.map(|entry| entry.key().to_vec()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            held,
            vec![short],
            "the receiving side must hold the short entry and not the long one"
        );
        Ok(())
    }

    /// A first frame announcing more than the opening ceiling is refused on
    /// its length prefix, while the peer holds the stream open and sends no
    /// body.
    #[tokio::test]
    async fn an_opening_frame_over_the_ceiling_is_refused_before_its_body() -> Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let (ours, theirs) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(ours);
        let (_their_reader, mut their_writer) = tokio::io::split(theirs);
        let announced = u32::try_from(MAX_OPENING_FRAME + 1)?;
        their_writer.write_all(&announced.to_be_bytes()).await?;

        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionOpening::read(writer, reader, peer),
        )
        .await;
        assert!(
            matches!(read, Ok(Err(_))),
            "the opening must be refused on its length, not wait for its body: {read:?}"
        );
        drop(their_writer);
        Ok(())
    }

    /// The first message of a replica whose first key is `MAX_KEY_BYTES`
    /// long — the largest an honest node sends — opens a session.
    #[tokio::test]
    async fn the_largest_honest_opening_fits_under_the_ceiling() -> Result<()> {
        let mut rng = rand::rng();
        let peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let mut store = store::Store::memory();
        let author = store.new_author(&mut rng)?;
        let namespace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(namespace.clone())?;
        replica
            .hash_and_insert(vec![b'k'; crate::sync::MAX_KEY_BYTES], &author, "v")
            .await?;
        let message = replica.sync_initial_message(None)?;
        drop(replica);

        // Room for the whole frame, so the write completes before the read.
        let (ours, theirs) = tokio::io::duplex(2 * MAX_OPENING_FRAME);
        let (reader, writer) = tokio::io::split(ours);
        let (_their_reader, their_writer) = tokio::io::split(theirs);
        let mut framed = FramedWrite::new(their_writer, SyncCodec::default());
        framed
            .send(super::Message::Init(super::Init {
                namespace: namespace.id(),
                identity: TEST_HOLDER,
                caller: TEST_HOLDER,
                message,
            }))
            .await?;

        let opening = SessionOpening::read(writer, reader, peer)
            .await
            .map_err(|err| anyhow!("the largest honest opening was refused: {err:?}"))?;
        assert_eq!(opening.namespace(), namespace.id());
        Ok(())
    }

    /// Past its opening a session reads frames larger than the opening
    /// ceiling: a replica that crosses in one message of that size reaches
    /// the empty side whole.
    #[tokio::test]
    async fn a_session_reads_frames_above_the_opening_ceiling_past_its_opening() -> Result<()> {
        let mut rng = rand::rng();
        let alice_peer = SecretKey::from_bytes(&[1u8; 32]).public();
        let bob_peer = SecretKey::from_bytes(&[2u8; 32]).public();
        let namespace = NamespaceSecret::new(&mut rng);

        // Sized past the ceiling: every entry carries a key of 400 bytes.
        let count = 2 * MAX_OPENING_FRAME / 400;
        let mut alice_store = store::Store::memory();
        let author = alice_store.new_author(&mut rng)?;
        let mut replica = alice_store.new_replica(namespace.clone())?;
        for i in 0..count {
            replica
                .hash_and_insert(format!("{i:0>400}"), &author, "v")
                .await?;
        }
        drop(replica);
        alice_store.close_replica(namespace.id());
        let mut bob_store = store::Store::memory();
        drop(bob_store.new_replica(namespace.clone())?);
        bob_store.close_replica(namespace.id());

        let alice = SyncHandle::spawn(alice_store, None, None, None, "alice".to_string());
        alice
            .open(namespace.id(), OpenOpts::default().sync())
            .await?;
        let bob = SyncHandle::spawn(bob_store, None, None, None, "bob".to_string());
        bob.open(namespace.id(), OpenOpts::default().sync()).await?;
        run_sync(
            alice.clone(),
            alice_peer,
            bob.clone(),
            bob_peer,
            namespace.id(),
        )
        .await?;
        let _alice_store = alice.shutdown().await?;
        let mut bob_store = bob.shutdown().await?;

        let held = bob_store.get_many(namespace.id(), Query::all())?.count();
        assert_eq!(held, count, "the replica did not cross whole");
        Ok(())
    }
}
