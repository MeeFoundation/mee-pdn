//! API for pdn-store replicas

// Names and concepts roughly follow Willow's design:
// https://hackmd.io/DTtck8QOQm6tZaQBBtTf7w

use std::{
    cmp::Ordering,
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use bytes::{Bytes, BytesMut};
// `iroh::Signature` is a newtype wrapper around `ed25519_dalek::Signature` with a
// hand-written, wire-stable `serialize_tuple` serde impl (the same impl every other
// iroh crate uses for handshake / discovery payloads). Embedding it inside
// `EntrySignature` — rather than the raw dalek type — keeps the on-wire
// `SignedEntry` format independent of upstream `ed25519` serde changes.
use iroh::{KeyParsingError, Signature, SignatureError};
use iroh_blobs::Hash;
use n0_future::time::{Duration, SystemTime};
use serde::{Deserialize, Serialize};

pub(crate) use crate::heads::AuthorHeads;
use crate::{
    keys::{Author, AuthorId, AuthorPublicKey, NamespaceId, NamespacePublicKey, NamespaceSecret},
    ranger::{
        self, Fingerprint, InsertOutcome, RangeEntry, RangeKey, RangeValue, Store, ValidateOutcome,
    },
    store::{self, fs::StoreInstance, PublicKeyStore},
    subscribers::{Delivery, LagNotice, Subscribers},
};

/// Protocol message for the set reconciliation protocol.
///
/// Can be serialized to bytes with [serde] to transfer between peers.
pub(crate) type ProtocolMessage = crate::ranger::Message<SignedEntry>;

/// Byte representation of an iroh `EndpointId`.
// TODO: Consider `iroh::EndpointId` instead of raw bytes (`iroh` re-exports it from `iroh-base`).
pub(crate) type PeerIdBytes = [u8; 32];

/// Max time in the future from our wall clock time that we accept entries for.
/// Value is 10 minutes.
pub(crate) const MAX_TIMESTAMP_FUTURE_SHIFT: u64 =
    10 * 60 * Duration::from_secs(1).as_micros() as u64;

/// The longest key a replica holds: a longer one is refused on a local
/// write and dropped at ingest, so the first message a replica sends —
/// its first key, twice — stays under the ceiling a peer reads it with.
pub(crate) const MAX_KEY_BYTES: usize = 8 * 1024;

/// Callback that may be set on a replica to determine the availability status for a content hash.
pub(crate) type ContentStatusCallback =
    Arc<dyn Fn(Hash) -> n0_future::boxed::BoxFuture<ContentStatus> + Send + Sync + 'static>;

/// Event emitted by sync when entries are added.
#[derive(derive_more::Debug, Clone)]
pub enum Event {
    /// A local entry has been added.
    LocalInsert {
        /// Document in which the entry was inserted.
        namespace: NamespaceId,
        /// Inserted entry.
        entry: SignedEntry,
    },
    /// A remote entry has been added.
    RemoteInsert {
        /// Document in which the entry was inserted.
        namespace: NamespaceId,
        /// Inserted entry.
        entry: SignedEntry,
        /// Peer that provided the inserted entry.
        /// Debug matches [`iroh::PublicKey::fmt_short`] (first 5 bytes, lower hex).
        #[debug("{}", hex::encode(&from[..5]))]
        from: PeerIdBytes,
        /// [`ContentStatus`] for this entry in the remote's replica.
        remote_content_status: ContentStatus,
    },
    /// Events were dropped since the last one received: the subscription's
    /// buffer was full. What they reported is in the replica by the time this
    /// arrives, so the subscriber reads the replica again.
    Lagged {
        /// Document whose events were dropped.
        namespace: NamespaceId,
    },
}

impl LagNotice for Event {
    fn lagged(&self) -> Self {
        let namespace = match self {
            Self::LocalInsert { namespace, .. }
            | Self::RemoteInsert { namespace, .. }
            | Self::Lagged { namespace } => *namespace,
        };
        Self::Lagged { namespace }
    }
}

/// Whether an entry was inserted locally or by a remote peer.
#[derive(derive_more::Debug, Clone)]
pub(crate) enum InsertOrigin {
    /// The entry was inserted locally.
    Local,
    /// The entry was received from the remote node identified by [`PeerIdBytes`].
    Sync {
        /// The peer from which we received this entry.
        /// Debug matches [`iroh::PublicKey::fmt_short`] (first 5 bytes, lower hex).
        #[debug("{}", hex::encode(&from[..5]))]
        from: PeerIdBytes,
        /// Whether the peer claims to have the content blob for this entry.
        remote_content_status: ContentStatus,
    },
}

/// Whether the content status is available on a node.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContentStatus {
    /// The content is completely available.
    Complete,
    /// The content is partially available.
    Incomplete,
    /// The content is missing.
    Missing,
}

/// Outcome of a sync operation.
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncOutcome {
    /// Timestamp of the latest entry for each author in the set we received.
    pub(crate) heads_received: AuthorHeads,
    /// Number of entries we received.
    pub(crate) num_recv: usize,
    /// Number of entries we sent.
    pub(crate) num_sent: usize,
}

/// Kind of capability of the namespace.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    num_enum::IntoPrimitive,
    num_enum::TryFromPrimitive,
    strum::Display,
)]
#[repr(u8)]
#[strum(serialize_all = "snake_case")]
pub enum CapabilityKind {
    /// A writable replica.
    Write = 1,
}

/// The capability of the namespace.
#[derive(Debug, Clone, Serialize, Deserialize, derive_more::From)]
///
/// An enum of one variant, so a ticket's encoding carries its variant index.
pub enum Capability {
    /// Write access to the namespace.
    Write(NamespaceSecret),
}

impl Capability {
    /// Get the [`NamespaceId`] for this [`Capability`].
    pub fn id(&self) -> NamespaceId {
        match self {
            Capability::Write(secret) => secret.id(),
        }
    }

    /// Get the [`NamespaceSecret`] of this [`Capability`].
    pub(crate) fn secret_key(&self) -> &NamespaceSecret {
        match self {
            Capability::Write(secret) => secret,
        }
    }

    /// Get the kind of capability.
    pub(crate) fn kind(&self) -> CapabilityKind {
        match self {
            Capability::Write(_) => CapabilityKind::Write,
        }
    }

    /// Get the raw representation of this namespace capability: the kind
    /// byte and the secret, as the namespaces table stores them.
    pub(crate) fn raw(&self) -> (u8, [u8; 32]) {
        let capability_repr: u8 = self.kind().into();
        let bytes = match self {
            Capability::Write(secret) => secret.to_bytes(),
        };
        (capability_repr, bytes)
    }

    /// Create a [`Capability`] from its raw representation; a kind byte no
    /// [`CapabilityKind`] carries is refused.
    pub(crate) fn from_raw(kind: u8, bytes: &[u8; 32]) -> anyhow::Result<Self> {
        let kind: CapabilityKind = kind.try_into()?;
        let capability = match kind {
            CapabilityKind::Write => Capability::Write(NamespaceSecret::from_bytes(bytes)),
        };
        Ok(capability)
    }
}

/// In memory information about an open replica.
#[derive(derive_more::Debug)]
pub(crate) struct ReplicaInfo {
    pub(crate) capability: Capability,
    subscribers: Subscribers<Event>,
    #[debug("ContentStatusCallback")]
    content_status_cb: Option<ContentStatusCallback>,
    closed: bool,
}

impl ReplicaInfo {
    /// Create a new replica.
    pub fn new(capability: Capability) -> Self {
        Self {
            capability,
            subscribers: Default::default(),
            // on_insert_sender: RwLock::new(None),
            content_status_cb: None,
            closed: false,
        }
    }

    /// Subscribe to insert events. A full channel drops them, and an
    /// [`Event::Lagged`] follows the last one received; the sender is the
    /// subscription's alone, or the lag accounting is off.
    pub fn subscribe(&mut self, sender: async_channel::Sender<Event>) {
        self.subscribers.subscribe(sender, Delivery::Lossy)
    }

    /// Inserts wait for room in the channel of a [`Delivery::Blocking`]
    /// subscriber.
    pub(crate) fn subscribe_with(
        &mut self,
        sender: async_channel::Sender<Event>,
        delivery: Delivery,
    ) {
        self.subscribers.subscribe(sender, delivery)
    }

    /// Explicitly unsubscribe a sender.
    ///
    /// Simply dropping the receiver is fine too. If you cloned a single sender to subscribe to
    /// multiple replicas, you can use this method to explicitly unsubscribe the sender from
    /// this replica without having to drop the receiver.
    pub(crate) fn unsubscribe(&mut self, sender: &async_channel::Sender<Event>) {
        self.subscribers.unsubscribe(sender)
    }

    /// Set the content status callback.
    ///
    /// Only one callback can be active at a time. If a previous callback was registered, this
    /// will return `false`.
    pub(crate) fn set_content_status_callback(&mut self, cb: ContentStatusCallback) -> bool {
        if self.content_status_cb.is_some() {
            false
        } else {
            self.content_status_cb = Some(cb);
            true
        }
    }

    fn ensure_open(&self) -> Result<(), InsertError> {
        if self.closed() {
            Err(InsertError::Closed)
        } else {
            Ok(())
        }
    }

    /// Returns true if the replica is closed.
    ///
    /// If a replica is closed, no further operations can be performed. A replica cannot be closed
    /// manually, it must be closed via [`store::Store::close_replica`] or
    /// [`store::Store::remove_replica`]
    pub fn closed(&self) -> bool {
        self.closed
    }
}

/// Local representation of a mutable, synchronizable key-value store.
#[derive(derive_more::Debug)]
pub(crate) struct Replica<'a, I = Box<ReplicaInfo>> {
    pub(crate) store: StoreInstance<'a>,
    pub(crate) info: I,
}

impl<'a, I> Replica<'a, I>
where
    I: Deref<Target = ReplicaInfo> + DerefMut,
{
    /// Create a new replica.
    pub fn new(store: StoreInstance<'a>, info: I) -> Self {
        Replica { info, store }
    }

    /// Insert a new record at the given key.
    ///
    /// The entry will by signed by the provided `author`.
    /// The `len` must be the byte length of the data identified by `hash`.
    ///
    /// Returns the number of entries removed as a consequence of this insertion,
    /// or an error either if the entry failed to validate or if a store operation failed.
    pub async fn insert(
        &mut self,
        key: impl AsRef<[u8]>,
        author: &Author,
        hash: Hash,
        len: u64,
    ) -> Result<usize, InsertError> {
        if len == 0 || hash == Hash::EMPTY {
            return Err(InsertError::EntryIsEmpty);
        }
        self.info.ensure_open()?;
        let id = RecordIdentifier::new(self.id(), author.id(), key);
        let record = Record::new_current(hash, len);
        let entry = Entry::new(id, record);
        let signed_entry = entry.sign(self.secret_key(), author);
        self.insert_entry(signed_entry, InsertOrigin::Local).await
    }

    /// Delete `author`'s entry at `key` by inserting an empty entry there;
    /// no other key is touched.
    ///
    /// Returns 1 when an older entry at `key` was replaced, 0 otherwise.
    pub(crate) async fn delete(
        &mut self,
        key: impl AsRef<[u8]>,
        author: &Author,
    ) -> Result<usize, InsertError> {
        self.info.ensure_open()?;
        let id = RecordIdentifier::new(self.id(), author.id(), key);
        let entry = Entry::new_empty(id);
        let signed_entry = entry.sign(self.secret_key(), author);
        self.insert_entry(signed_entry, InsertOrigin::Local).await
    }

    /// Insert an entry into this replica which was received from a remote peer.
    ///
    /// This will verify both the namespace and author signatures of the entry, insert the entry
    /// into the replica store, and emit an [`Event::RemoteInsert`].
    ///
    /// Returns the number of entries removed as a consequence of this insertion,
    /// or an error if the entry failed to validate or if a store operation failed.
    #[cfg(test)]
    pub(crate) async fn insert_remote_entry(
        &mut self,
        entry: SignedEntry,
        received_from: PeerIdBytes,
        content_status: ContentStatus,
    ) -> Result<usize, InsertError> {
        self.info.ensure_open()?;
        let origin = InsertOrigin::Sync {
            from: received_from,
            remote_content_status: content_status,
        };
        self.insert_entry(entry, origin).await
    }

    /// Insert a signed entry into the database.
    ///
    /// Returns the number of entries removed as a consequence of this insertion.
    async fn insert_entry(
        &mut self,
        entry: SignedEntry,
        origin: InsertOrigin,
    ) -> Result<usize, InsertError> {
        let namespace = self.id();

        let store = &self.store;
        validate_entry(system_time_now(), store, namespace, &entry, &origin, None)?;

        let outcome = self.store.put(entry.clone()).map_err(InsertError::Store)?;
        tracing::debug!(?origin, hash = %entry.content_hash(), ?outcome, "insert");

        let removed_count = match outcome {
            InsertOutcome::Inserted { removed } => removed,
            InsertOutcome::NotInserted => return Err(InsertError::NewerEntryExists),
        };

        let insert_event = match origin {
            InsertOrigin::Local => Event::LocalInsert { namespace, entry },
            InsertOrigin::Sync {
                from,
                remote_content_status,
            } => Event::RemoteInsert {
                namespace,
                entry,
                from,
                remote_content_status,
            },
        };

        self.info.subscribers.send(insert_event).await;

        Ok(removed_count)
    }

    /// Hashes the given data and inserts it.
    ///
    /// This does not store the content, just the record of it.
    /// Returns the calculated hash.
    #[cfg(test)]
    pub(crate) async fn hash_and_insert(
        &mut self,
        key: impl AsRef<[u8]>,
        author: &Author,
        data: impl AsRef<[u8]>,
    ) -> Result<Hash, InsertError> {
        self.info.ensure_open()?;
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        self.insert(key, author, hash, len).await?;
        Ok(hash)
    }

    /// Create the initial message for the set reconciliation flow with a remote peer.
    ///
    /// With a session `filter` set, the initial range boundary and
    /// fingerprint derive from the filtered view only.
    pub(crate) fn sync_initial_message(
        &mut self,
        filter: Option<crate::filter::EntryFilter>,
    ) -> anyhow::Result<crate::ranger::Message<SignedEntry>> {
        self.info.ensure_open().map_err(anyhow::Error::from)?;
        crate::filter::SessionStore::new(&mut self.store, filter).initial_message()
    }

    /// Process a set reconciliation message from a remote peer.
    ///
    /// Returns the next message to be sent to the peer, if any.
    ///
    /// With a session `filter` set, every read this round performs —
    /// fingerprints, split boundaries, item transmissions — sees only the
    /// entries the filter admits, so nothing else is revealed to the peer.
    /// Ingest of the peer's entries is unaffected: writes pass through the
    /// session store, gated by `validate_entry` alone.
    pub(crate) async fn sync_process_message(
        &mut self,
        message: crate::ranger::Message<SignedEntry>,
        from_peer: PeerIdBytes,
        state: &mut SyncOutcome,
        filter: Option<crate::filter::EntryFilter>,
        ingest: Option<crate::filter::SessionIngest>,
    ) -> Result<Option<crate::ranger::Message<SignedEntry>>, anyhow::Error> {
        self.info.ensure_open()?;
        let my_namespace = self.id();
        let now = system_time_now();

        // update state with incoming data.
        state.num_recv += message.value_count();
        for (entry, _content_status) in message.values() {
            state
                .heads_received
                .insert(entry.author(), entry.timestamp());
        }

        let cb = self.info.content_status_cb.clone();

        let mut session_store = crate::filter::SessionStore::new(&mut self.store, filter);
        let reply = session_store
            .process_message(
                &Default::default(),
                message,
                // validate callback: validate incoming entries, and send to on_insert channel
                |store, entry, content_status| {
                    let origin = InsertOrigin::Sync {
                        from: from_peer,
                        remote_content_status: content_status,
                    };
                    // here: the session's ingest verdict. Every failure — intrinsic
                    // (signature, clock skew) or the verdict's own — stays quiet, and
                    // the sender offers the entry again in a later session.
                    match validate_entry(now, store, my_namespace, entry, &origin, ingest.as_ref())
                    {
                        Ok(()) => ValidateOutcome::Accept,
                        Err(_) => ValidateOutcome::Drop,
                    }
                },
                // on_insert callback: is called when an entry was actually inserted in the store
                async |_store, entry, content_status| {
                    // We use `send_with` to only clone the entry if we have active subscriptions.
                    self.info
                        .subscribers
                        .send_with(|| Event::RemoteInsert {
                            from: from_peer,
                            namespace: my_namespace,
                            entry: entry.clone(),
                            remote_content_status: content_status,
                        })
                        .await
                },
                // content_status callback: get content status for outgoing entries
                async move |entry| {
                    if let Some(cb) = cb.as_ref() {
                        cb(entry.content_hash()).await
                    } else {
                        ContentStatus::Missing
                    }
                },
            )
            .await?;

        // update state with outgoing data.
        if let Some(ref reply) = reply {
            state.num_sent += reply.value_count();
        }

        Ok(reply)
    }

    /// Get the namespace identifier for this [`Replica`].
    pub fn id(&self) -> NamespaceId {
        self.info.capability.id()
    }

    /// Get the [`NamespaceSecret`] key for this replica.
    pub fn secret_key(&self) -> &NamespaceSecret {
        self.info.capability.secret_key()
    }
}

/// Validate a [`SignedEntry`] if it's fit to be inserted.
///
/// This validates that
/// * the entry's author and namespace signatures are correct
/// * the entry's namespace matches the current replica
/// * the entry's timestamp is not more than 10 minutes in the future of our system time
/// * the entry's key is no longer than [`MAX_KEY_BYTES`]
/// * the entry has the empty hash exactly when it has length 0
/// * a remote entry passes `ingest`, the session's verdict, when one is given.
///
/// Whether the entry is newer than the one held at its key and author is the
/// store's `put` to decide.
fn validate_entry<S: ranger::Store<SignedEntry> + PublicKeyStore>(
    now: u64,
    store: &S,
    expected_namespace: NamespaceId,
    entry: &SignedEntry,
    origin: &InsertOrigin,
    ingest: Option<&crate::filter::SessionIngest>,
) -> Result<(), ValidationFailure> {
    // Verify the namespace
    if entry.namespace() != expected_namespace {
        return Err(ValidationFailure::InvalidNamespace);
    }

    if entry.key().len() > MAX_KEY_BYTES {
        return Err(ValidationFailure::KeyTooLong);
    }

    // An empty entry is the empty hash and length 0 together: readers that
    // judge emptiness by one of the two would otherwise read one entry two ways.
    entry.validate_empty()?;

    // Verify signature for non-local entries.
    if !matches!(origin, InsertOrigin::Local) && entry.verify(store).is_err() {
        return Err(ValidationFailure::BadSignature);
    }

    // Verify that the timestamp of the entry is not too far in the future.
    if entry.timestamp() > now + MAX_TIMESTAMP_FUTURE_SHIFT {
        return Err(ValidationFailure::TooFarInTheFuture);
    }

    // here: the session's ingest verdict — remote entries only, mirroring
    // the signature check above; decided at the session's setup, it holds
    // for that session alone. An insert outside any session takes none.
    if let (InsertOrigin::Sync { .. }, Some(ingest)) = (origin, ingest) {
        if ingest(entry) == ValidateOutcome::Drop {
            return Err(ValidationFailure::NotAdmitted);
        }
    }
    Ok(())
}

/// Error emitted when inserting entries into a [`Replica`] failed
#[derive(thiserror::Error, derive_more::Debug)]
pub(crate) enum InsertError {
    /// Storage error
    #[error("storage error")]
    Store(anyhow::Error),
    /// Validation failure
    #[error("validation failure")]
    Validation(#[from] ValidationFailure),
    /// A newer entry exists at this entry's key.
    #[error("A newer entry exists at this entry's key.")]
    NewerEntryExists,
    /// Attempted to insert an empty entry.
    #[error("Attempted to insert an empty entry")]
    EntryIsEmpty,
    /// The replica is closed, no operations may be performed.
    #[error("replica is closed")]
    Closed,
}

/// Reason why verifying a [`SignedEntry`] failed.
#[derive(thiserror::Error, Debug)]
pub(crate) enum SignedEntryVerifyError {
    /// One of the entry's public key bytes is not a valid ed25519 curve point.
    #[error(transparent)]
    KeyParsing(#[from] KeyParsingError),
    /// One of the entry's signatures failed verification against the recovered key.
    #[error(transparent)]
    Signature(#[from] SignatureError),
}

/// Reason why entry validation failed
#[derive(thiserror::Error, Debug)]
pub(crate) enum ValidationFailure {
    /// Entry namespace does not match the current replica.
    #[error("Entry namespace does not match the current replica")]
    InvalidNamespace,
    /// Entry signature is invalid.
    #[error("Entry signature is invalid")]
    BadSignature,
    /// Entry timestamp is too far in the future.
    #[error("Entry timestamp is too far in the future.")]
    TooFarInTheFuture,
    /// Entry key is longer than [`MAX_KEY_BYTES`].
    #[error("Entry key is longer than {MAX_KEY_BYTES} bytes")]
    KeyTooLong,
    /// Entry has length 0 but not the empty hash, or the empty hash but not length 0.
    #[error("Entry has length 0 but not the empty hash, or the empty hash but not length 0")]
    InvalidEmptyEntry,
    /// The session's ingest verdict kept the entry out; the sender is not
    /// told.
    #[error("Entry was not admitted")]
    NotAdmitted,
}

/// A signed entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedEntry {
    signature: EntrySignature,
    entry: Entry,
}

impl From<SignedEntry> for Entry {
    fn from(value: SignedEntry) -> Self {
        value.entry
    }
}

impl PartialOrd for SignedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SignedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.entry.cmp(&other.entry)
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id
            .cmp(&other.id)
            .then_with(|| self.record.cmp(&other.record))
    }
}

impl SignedEntry {
    pub(crate) fn new(signature: EntrySignature, entry: Entry) -> Self {
        SignedEntry { signature, entry }
    }

    /// Create a new signed entry by signing an entry with the `namespace` and `author`.
    pub(crate) fn from_entry(entry: Entry, namespace: &NamespaceSecret, author: &Author) -> Self {
        let signature = EntrySignature::from_entry(&entry, namespace, author);
        SignedEntry { signature, entry }
    }

    /// Create a new signed entries from its parts.
    pub fn from_parts(
        namespace: &NamespaceSecret,
        author: &Author,
        key: impl AsRef<[u8]>,
        record: Record,
    ) -> Self {
        let id = RecordIdentifier::new(namespace.id(), author.id(), key);
        let entry = Entry::new(id, record);
        Self::from_entry(entry, namespace, author)
    }

    /// Verify the signatures on this entry.
    pub(crate) fn verify<S: store::PublicKeyStore>(
        &self,
        store: &S,
    ) -> Result<(), SignedEntryVerifyError> {
        self.signature.verify(
            &self.entry,
            &self.entry.namespace().public_key(store)?,
            &self.entry.author().public_key(store)?,
        )?;
        Ok(())
    }

    /// Get the signature.
    pub(crate) fn signature(&self) -> &EntrySignature {
        &self.signature
    }

    /// Validate that the entry has the empty hash if the length is 0, or a non-zero length.
    pub(crate) fn validate_empty(&self) -> Result<(), ValidationFailure> {
        self.entry().validate_empty()
    }

    /// Get the [`Entry`].
    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    /// Get the content [`struct@Hash`] of the entry.
    pub fn content_hash(&self) -> Hash {
        self.entry().content_hash()
    }

    /// Get the content length of the entry.
    pub fn content_len(&self) -> u64 {
        self.entry().content_len()
    }

    /// Get the author bytes of this entry.
    pub(crate) fn author_bytes(&self) -> AuthorId {
        self.entry().id().author()
    }

    /// Get the key of the entry.
    pub fn key(&self) -> &[u8] {
        self.entry().id().key()
    }

    /// Get the timestamp of the entry.
    pub fn timestamp(&self) -> u64 {
        self.entry().timestamp()
    }
}

impl RangeEntry for SignedEntry {
    type Key = RecordIdentifier;
    type Value = Record;

    fn key(&self) -> &Self::Key {
        &self.entry.id
    }

    fn value(&self) -> &Self::Value {
        &self.entry.record
    }

    fn as_fingerprint(&self) -> crate::ranger::Fingerprint {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.namespace().as_ref());
        hasher.update(self.author_bytes().as_ref());
        hasher.update(self.key());
        hasher.update(&self.timestamp().to_be_bytes());
        hasher.update(self.content_hash().as_bytes());
        Fingerprint(hasher.finalize().into())
    }
}

/// Signature over an entry.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EntrySignature {
    author_signature: Signature,
    namespace_signature: Signature,
}

impl Debug for EntrySignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntrySignature")
            .field(
                "namespace_signature",
                &hex::encode(self.namespace_signature.to_bytes()),
            )
            .field(
                "author_signature",
                &hex::encode(self.author_signature.to_bytes()),
            )
            .finish()
    }
}

impl EntrySignature {
    /// Create a new signature by signing an entry with the `namespace` and `author`.
    pub(crate) fn from_entry(entry: &Entry, namespace: &NamespaceSecret, author: &Author) -> Self {
        // TODO: this should probably include a namespace prefix
        // namespace in the cryptographic sense.
        let bytes = entry.to_vec();
        let namespace_signature = Signature::from_bytes(&namespace.sign(&bytes).to_bytes());
        let author_signature = Signature::from_bytes(&author.sign(&bytes).to_bytes());

        EntrySignature {
            author_signature,
            namespace_signature,
        }
    }

    /// Verify that this signature was created by signing the `entry` with the
    /// secret keys of the specified `author` and `namespace`.
    pub fn verify(
        &self,
        entry: &Entry,
        namespace: &NamespacePublicKey,
        author: &AuthorPublicKey,
    ) -> Result<(), SignatureError> {
        let bytes = entry.to_vec();
        namespace.verify(&bytes, &self.namespace_signature)?;
        author.verify(&bytes, &self.author_signature)?;

        Ok(())
    }

    pub(crate) fn from_parts(namespace_sig: &[u8; 64], author_sig: &[u8; 64]) -> Self {
        let namespace_signature = Signature::from_bytes(namespace_sig);
        let author_signature = Signature::from_bytes(author_sig);

        EntrySignature {
            author_signature,
            namespace_signature,
        }
    }

    pub(crate) fn author(&self) -> &Signature {
        &self.author_signature
    }

    pub(crate) fn namespace(&self) -> &Signature {
        &self.namespace_signature
    }
}

/// A single entry in a `Replica`
///
/// An entry is identified by a key, its [`AuthorId`], and the `Replica`'s
/// [`NamespaceId`]. Its value is the [32-byte BLAKE3 hash](iroh_blobs::Hash)
/// of the entry's content data, the size of this content data, and a timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    id: RecordIdentifier,
    record: Record,
}

impl Entry {
    /// Create a new entry
    pub fn new(id: RecordIdentifier, record: Record) -> Self {
        Entry { id, record }
    }

    /// Create a new empty entry with the current timestamp.
    pub(crate) fn new_empty(id: RecordIdentifier) -> Self {
        Entry {
            id,
            record: Record::empty_current(),
        }
    }

    /// Validate that the entry has the empty hash if the length is 0, or a non-zero length.
    pub(crate) fn validate_empty(&self) -> Result<(), ValidationFailure> {
        match (self.content_hash() == Hash::EMPTY, self.content_len() == 0) {
            (true, true) => Ok(()),
            (false, false) => Ok(()),
            (true, false) => Err(ValidationFailure::InvalidEmptyEntry),
            (false, true) => Err(ValidationFailure::InvalidEmptyEntry),
        }
    }

    /// Get the [`RecordIdentifier`] for this entry.
    pub fn id(&self) -> &RecordIdentifier {
        &self.id
    }

    /// Get the [`NamespaceId`] of this entry.
    pub fn namespace(&self) -> NamespaceId {
        self.id.namespace()
    }

    /// Get the [`AuthorId`] of this entry.
    pub fn author(&self) -> AuthorId {
        self.id.author()
    }

    /// Get the key of this entry.
    pub fn key(&self) -> &[u8] {
        self.id.key()
    }

    /// Get the [`Record`] contained in this entry.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Get the content hash of the record.
    pub fn content_hash(&self) -> Hash {
        self.record.hash
    }

    /// Get the content length of the record.
    pub fn content_len(&self) -> u64 {
        self.record.len
    }

    /// Get the timestamp of the record.
    pub fn timestamp(&self) -> u64 {
        self.record.timestamp
    }

    /// Serialize this entry into its canonical byte representation used for signing.
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.id.encode(out);
        self.record.encode(out);
    }

    /// Serialize this entry into a new vector with its canonical byte representation.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Sign this entry with a [`NamespaceSecret`] and [`Author`].
    pub fn sign(self, namespace: &NamespaceSecret, author: &Author) -> SignedEntry {
        SignedEntry::from_entry(self, namespace, author)
    }
}

const NAMESPACE_BYTES: std::ops::Range<usize> = 0..32;
const AUTHOR_BYTES: std::ops::Range<usize> = 32..64;
const KEY_BYTES: std::ops::RangeFrom<usize> = 64..;

/// The identifier of a record.
///
/// It opens with a 32-byte namespace and a 32-byte author, and every
/// accessor slices both unchecked, so a value lacking them must not exist.
/// [`new`](Self::new) always writes them; deserialization is hand-written
/// to refuse anything shorter, because that is the one way a value enters
/// from outside this crate — a range boundary or an entry off the wire.
#[derive(Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordIdentifier(Bytes);

/// Namespace and author: the fixed head every record identifier carries.
pub(crate) const ID_HEAD_BYTES: usize = AUTHOR_BYTES.end;

/// The one constructor for an identifier whose bytes come from outside.
fn record_identifier_from_bytes<E: serde::de::Error>(bytes: Bytes) -> Result<RecordIdentifier, E> {
    if bytes.len() < ID_HEAD_BYTES {
        return Err(E::invalid_length(
            bytes.len(),
            &"a record identifier carrying a 32-byte namespace and a 32-byte author",
        ));
    }
    Ok(RecordIdentifier(bytes))
}

impl<'de> Deserialize<'de> for RecordIdentifier {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RecordIdentifierVisitor;

        impl<'de> serde::de::Visitor<'de> for RecordIdentifierVisitor {
            type Value = RecordIdentifier;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a record identifier of at least {ID_HEAD_BYTES} bytes")
            }

            // Both shapes serde's derive accepts for a newtype struct, so
            // every format decodes the same value the derive would.
            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                record_identifier_from_bytes(Bytes::deserialize(deserializer)?)
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let bytes = seq
                    .next_element::<Bytes>()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                record_identifier_from_bytes(bytes)
            }
        }

        deserializer.deserialize_newtype_struct("RecordIdentifier", RecordIdentifierVisitor)
    }
}

impl Default for RecordIdentifier {
    fn default() -> Self {
        Self::new(NamespaceId::default(), AuthorId::default(), b"")
    }
}

impl Debug for RecordIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordIdentifier")
            .field("namespace", &self.namespace())
            .field("author", &self.author())
            .field("key", &std::string::String::from_utf8_lossy(self.key()))
            .finish()
    }
}

impl RangeKey for RecordIdentifier {
    #[cfg(test)]
    fn is_prefix_of(&self, other: &Self) -> bool {
        other.as_ref().starts_with(self.as_ref())
    }
}

pub(crate) fn system_time_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time drift")
        .as_micros() as u64
}

impl RecordIdentifier {
    /// Create a new [`RecordIdentifier`].
    pub fn new(
        namespace: impl Into<NamespaceId>,
        author: impl Into<AuthorId>,
        key: impl AsRef<[u8]>,
    ) -> Self {
        let mut bytes = BytesMut::with_capacity(32 + 32 + key.as_ref().len());
        bytes.extend_from_slice(namespace.into().as_bytes());
        bytes.extend_from_slice(author.into().as_bytes());
        bytes.extend_from_slice(key.as_ref());
        Self(bytes.freeze())
    }

    /// Serialize this [`RecordIdentifier`] into a mutable byte array.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }

    /// Get this [`RecordIdentifier`] as [Bytes].
    pub fn as_bytes(&self) -> Bytes {
        self.0.clone()
    }

    /// Get this [`RecordIdentifier`] as a tuple of byte slices.
    #[cfg(test)]
    pub(crate) fn as_byte_tuple(&self) -> (&[u8; 32], &[u8; 32], &[u8]) {
        (
            self.0[NAMESPACE_BYTES].try_into().unwrap(),
            self.0[AUTHOR_BYTES].try_into().unwrap(),
            &self.0[KEY_BYTES],
        )
    }

    /// Get this [`RecordIdentifier`] as a tuple of bytes.
    pub(crate) fn to_byte_tuple(&self) -> ([u8; 32], [u8; 32], Bytes) {
        (
            self.0[NAMESPACE_BYTES].try_into().unwrap(),
            self.0[AUTHOR_BYTES].try_into().unwrap(),
            self.0.slice(KEY_BYTES),
        )
    }

    /// Get the key of this record.
    pub fn key(&self) -> &[u8] {
        &self.0[KEY_BYTES]
    }

    /// Get the key of this record as [`Bytes`].
    #[cfg(test)]
    pub(crate) fn key_bytes(&self) -> Bytes {
        self.0.slice(KEY_BYTES)
    }

    /// Get the [`NamespaceId`] of this record as byte array.
    pub fn namespace(&self) -> NamespaceId {
        let value: &[u8; 32] = &self.0[NAMESPACE_BYTES].try_into().unwrap();
        value.into()
    }

    /// Get the [`AuthorId`] of this record as byte array.
    pub fn author(&self) -> AuthorId {
        let value: &[u8; 32] = &self.0[AUTHOR_BYTES].try_into().unwrap();
        value.into()
    }
}

impl AsRef<[u8]> for RecordIdentifier {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Deref for SignedEntry {
    type Target = Entry;
    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl Deref for Entry {
    type Target = Record;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

/// The data part of an entry in a `Replica`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    /// Length of the data referenced by `hash`.
    len: u64,
    /// Hash of the content data.
    hash: Hash,
    /// Record creation timestamp. Counted as micros since the Unix epoch.
    timestamp: u64,
}

impl RangeValue for Record {}

/// Ordering for entry values.
///
/// Compares first the timestamp, then the content hash.
impl Ord for Record {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp
            .cmp(&other.timestamp)
            .then_with(|| self.hash.cmp(&other.hash))
    }
}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Record {
    /// Create a new record.
    pub fn new(hash: Hash, len: u64, timestamp: u64) -> Self {
        debug_assert!(
            len != 0 || hash == Hash::EMPTY,
            "if `len` is 0 then `hash` must be the hash of the empty byte range"
        );
        Record {
            hash,
            len,
            timestamp,
        }
    }

    /// A record as stored or sent: neither a store file nor the wire checks
    /// hash against length, and a store written before ingest checked it
    /// can hold a record whose two disagree (`validate_entry`).
    pub(crate) fn unchecked(hash: Hash, len: u64, timestamp: u64) -> Self {
        Record {
            hash,
            len,
            timestamp,
        }
    }

    /// Create a tombstone record (empty content)
    pub fn empty(timestamp: u64) -> Self {
        Self::new(Hash::EMPTY, 0, timestamp)
    }

    /// Create a tombstone record with the timestamp set to now.
    pub(crate) fn empty_current() -> Self {
        Self::new_current(Hash::EMPTY, 0)
    }

    /// Return `true` if the entry is empty.
    pub fn is_empty(&self) -> bool {
        self.hash == Hash::EMPTY
    }

    /// Create a new [`Record`] with the timestamp set to now.
    pub fn new_current(hash: Hash, len: u64) -> Self {
        let timestamp = system_time_now();
        Self::new(hash, len, timestamp)
    }

    /// Get the length of the data addressed by this record's content hash.
    pub fn content_len(&self) -> u64 {
        self.len
    }

    /// Get the [`struct@Hash`] of the content data of this record.
    pub fn content_hash(&self) -> Hash {
        self.hash
    }

    /// Get the timestamp of this record.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    #[cfg(test)]
    pub(crate) fn current_from_data(data: impl AsRef<[u8]>) -> Self {
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        Self::new_current(hash, len)
    }

    #[cfg(test)]
    pub(crate) fn from_data(data: impl AsRef<[u8]>, timestamp: u64) -> Self {
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        Self::new(hash, len, timestamp)
    }

    /// Serialize this record into a mutable byte array.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.len.to_be_bytes());
        out.extend_from_slice(self.hash.as_ref());
        out.extend_from_slice(&self.timestamp.to_be_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use anyhow::{Context as _, Result};
    use rand::SeedableRng;

    use super::*;

    /// A cache big enough that nothing in a scenario evicts.
    const TEST_CACHE_BYTES: usize = 16 * 1024 * 1024;
    use crate::{
        ranger::{Range, Store as _},
        store::{OpenError, Query, Store},
    };

    /// A replica's entries read back by key, author, prefix and range, one
    /// entry per author at a key, and the store keeps the peers a replica
    /// synced with as a cache of `PEERS_PER_DOC_CACHE_SIZE`, the newest first.
    #[tokio::test]
    async fn test_basics_memory() -> Result<()> {
        let store = store::Store::memory();
        test_basics(store).await?;

        Ok(())
    }

    /// The fs-store twin of `test_basics_memory`.
    #[tokio::test]
    async fn test_basics_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_basics(store).await?;
        Ok(())
    }

    async fn test_basics(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let alice = Author::new(&mut rng);
        let bob = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);

        let record_id = RecordIdentifier::new(myspace.id(), alice.id(), "/my/key");
        let record = Record::current_from_data(b"this is my cool data");
        let entry = Entry::new(record_id, record);
        let signed_entry = entry.sign(&myspace, &alice);
        signed_entry.verify(&()).expect("failed to verify");

        let mut my_replica = store.new_replica(myspace.clone())?;
        for i in 0..10 {
            my_replica
                .hash_and_insert(format!("/{i}"), &alice, format!("{i}: hello from alice"))
                .await?;
        }

        for i in 0..10 {
            let res = store
                .get_exact(myspace.id(), alice.id(), format!("/{i}"), false)?
                .unwrap();
            let len = format!("{i}: hello from alice").as_bytes().len() as u64;
            assert_eq!(res.entry().record().content_len(), len);
            res.verify(&())?;
        }

        // Test multiple records for the same key
        let mut my_replica = store.new_replica(myspace.clone())?;
        my_replica
            .hash_and_insert("/cool/path", &alice, "round 1")
            .await?;
        let _entry = store
            .get_exact(myspace.id(), alice.id(), "/cool/path", false)?
            .unwrap();
        // Second
        let mut my_replica = store.new_replica(myspace.clone())?;
        my_replica
            .hash_and_insert("/cool/path", &alice, "round 2")
            .await?;
        let _entry = store
            .get_exact(myspace.id(), alice.id(), "/cool/path", false)?
            .unwrap();

        // Get All by author
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(alice.id()))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 11);

        // Get All by author
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(bob.id()))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 0);

        // Get All by key
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::key_exact(b"/cool/path"))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 1);

        // Get All
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::all())?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 11);

        // insert record from different author
        let mut my_replica = store.new_replica(myspace.clone())?;
        let _entry = my_replica
            .hash_and_insert("/cool/path", &bob, "bob round 1")
            .await?;

        // Get All by author
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(alice.id()))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 11);

        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(bob.id()))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 1);

        // Get All by key
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::key_exact(b"/cool/path"))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 2);

        // Get all by prefix
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::key_prefix(b"/cool"))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 2);

        // Get All by author and prefix
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(alice.id()).key_prefix(b"/cool"))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 1);

        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::author(bob.id()).key_prefix(b"/cool"))?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 1);

        // Get All
        let entries: Vec<_> = store
            .get_many(myspace.id(), Query::all())?
            .collect::<Result<_>>()?;
        assert_eq!(entries.len(), 12);

        // Get Range of all should return all latest
        let mut my_replica = store.new_replica(myspace.clone())?;
        let entries_second: Vec<_> = my_replica
            .store
            .get_range(Range::new(
                RecordIdentifier::default(),
                RecordIdentifier::default(),
            ))?
            .collect::<Result<_, _>>()?;

        assert_eq!(entries_second.len(), 12);
        assert_eq!(entries, entries_second.into_iter().collect::<Vec<_>>());

        test_lru_cache_like_behaviour(&mut store, myspace.id())?;
        store.flush()?;
        Ok(())
    }

    /// Test that [`Store::register_useful_peer`] behaves like a LRUCache of size
    /// [`super::store::PEERS_PER_DOC_CACHE_SIZE`].
    fn test_lru_cache_like_behaviour(store: &mut Store, namespace: NamespaceId) -> Result<()> {
        /// Helper to verify the store returns the expected peers for the namespace.
        #[track_caller]
        fn verify_peers(store: &mut Store, namespace: NamespaceId, expected_peers: &Vec<[u8; 32]>) {
            assert_eq!(
                expected_peers,
                &store
                    .get_sync_peers(&namespace)
                    .unwrap()
                    .unwrap()
                    .collect::<Vec<_>>(),
                "sync peers differ"
            );
        }

        let count = super::store::PEERS_PER_DOC_CACHE_SIZE.get();
        // expected peers: newest peers are to the front, oldest to the back
        let mut expected_peers = Vec::with_capacity(count);
        for i in 0..count as u8 {
            let peer = [i; 32];
            expected_peers.insert(0, peer);
            store.register_useful_peer(namespace, peer)?;
        }
        verify_peers(store, namespace, &expected_peers);

        // one more peer should evict the last peer
        expected_peers.pop();
        let newer_peer = [count as u8; 32];
        expected_peers.insert(0, newer_peer);
        store.register_useful_peer(namespace, newer_peer)?;
        verify_peers(store, namespace, &expected_peers);

        // move one existing peer up
        let refreshed_peer = expected_peers.remove(2);
        expected_peers.insert(0, refreshed_peer);
        store.register_useful_peer(namespace, refreshed_peer)?;
        verify_peers(store, namespace, &expected_peers);
        Ok(())
    }

    /// The store's content hashes are those of every entry of every replica
    /// it holds.
    #[tokio::test]
    async fn test_content_hashes_iterator_memory() -> Result<()> {
        let store = store::Store::memory();
        test_content_hashes_iterator(store).await
    }

    /// The fs-store twin of `test_content_hashes_iterator_memory`.
    #[tokio::test]
    async fn test_content_hashes_iterator_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_content_hashes_iterator(store).await
    }

    async fn test_content_hashes_iterator(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let mut expected = HashSet::new();
        let n_replicas = 3;
        let n_entries = 4;
        for i in 0..n_replicas {
            let namespace = NamespaceSecret::new(&mut rng);
            let author = store.new_author(&mut rng)?;
            let mut replica = store.new_replica(namespace)?;
            for j in 0..n_entries {
                let key = format!("{j}");
                let data = format!("{i}:{j}");
                let hash = replica.hash_and_insert(key, &author, data).await?;
                expected.insert(hash);
            }
        }
        assert_eq!(expected.len(), n_replicas * n_entries);
        let actual = store.content_hashes()?.collect::<Result<HashSet<Hash>>>()?;
        assert_eq!(actual, expected);
        Ok(())
    }

    /// Record identifiers order by namespace, then author, then key, and a
    /// range over them holds its start and leaves out its end.
    #[test]
    fn test_multikey() {
        let mut rng = rand::rng();

        let k = ["a", "c", "z"];

        let mut n: Vec<_> = (0..3).map(|_| NamespaceSecret::new(&mut rng)).collect();
        n.sort_by_key(|n| n.id());

        let mut a: Vec<_> = (0..3).map(|_| Author::new(&mut rng)).collect();
        a.sort_by_key(|a| a.id());

        // Just key
        {
            let ri0 = RecordIdentifier::new(n[0].id(), a[0].id(), k[0]);
            let ri1 = RecordIdentifier::new(n[0].id(), a[0].id(), k[1]);
            let ri2 = RecordIdentifier::new(n[0].id(), a[0].id(), k[2]);

            let range = Range::new(ri0.clone(), ri2.clone());
            assert!(range.contains(&ri0), "start");
            assert!(range.contains(&ri1), "inside");
            assert!(!range.contains(&ri2), "end");

            assert!(ri0 < ri1);
            assert!(ri1 < ri2);
        }

        // Just namespace
        {
            let ri0 = RecordIdentifier::new(n[0].id(), a[0].id(), k[0]);
            let ri1 = RecordIdentifier::new(n[1].id(), a[0].id(), k[1]);
            let ri2 = RecordIdentifier::new(n[2].id(), a[0].id(), k[2]);

            let range = Range::new(ri0.clone(), ri2.clone());
            assert!(range.contains(&ri0), "start");
            assert!(range.contains(&ri1), "inside");
            assert!(!range.contains(&ri2), "end");

            assert!(ri0 < ri1);
            assert!(ri1 < ri2);
        }

        // Just author
        {
            let ri0 = RecordIdentifier::new(n[0].id(), a[0].id(), k[0]);
            let ri1 = RecordIdentifier::new(n[0].id(), a[1].id(), k[0]);
            let ri2 = RecordIdentifier::new(n[0].id(), a[2].id(), k[0]);

            let range = Range::new(ri0.clone(), ri2.clone());
            assert!(range.contains(&ri0), "start");
            assert!(range.contains(&ri1), "inside");
            assert!(!range.contains(&ri2), "end");

            assert!(ri0 < ri1);
            assert!(ri1 < ri2);
        }

        // Just key and namespace
        {
            let ri0 = RecordIdentifier::new(n[0].id(), a[0].id(), k[0]);
            let ri1 = RecordIdentifier::new(n[1].id(), a[0].id(), k[1]);
            let ri2 = RecordIdentifier::new(n[2].id(), a[0].id(), k[2]);

            let range = Range::new(ri0.clone(), ri2.clone());
            assert!(range.contains(&ri0), "start");
            assert!(range.contains(&ri1), "inside");
            assert!(!range.contains(&ri2), "end");

            assert!(ri0 < ri1);
            assert!(ri1 < ri2);
        }

        // Mixed
        {
            // Ord should prioritize namespace - author - key

            let a0 = a[0].id();
            let a1 = a[1].id();
            let n0 = n[0].id();
            let n1 = n[1].id();
            let k0 = k[0];
            let k1 = k[1];

            assert!(RecordIdentifier::new(n0, a0, k0) < RecordIdentifier::new(n1, a1, k1));
            assert!(RecordIdentifier::new(n0, a0, k1) < RecordIdentifier::new(n1, a0, k0));
            assert!(RecordIdentifier::new(n0, a1, k0) < RecordIdentifier::new(n0, a1, k1));
            assert!(RecordIdentifier::new(n1, a1, k0) < RecordIdentifier::new(n1, a1, k1));
        }
    }

    /// A local write older than the entry at its author's key is refused as
    /// `NewerEntryExists`, and the newer entry stays.
    #[tokio::test]
    async fn test_timestamps_memory() -> Result<()> {
        let store = store::Store::memory();
        test_timestamps(store).await?;

        Ok(())
    }

    /// The fs-store twin of `test_timestamps_memory`.
    #[tokio::test]
    async fn test_timestamps_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_timestamps(store).await?;
        Ok(())
    }

    async fn test_timestamps(mut store: Store) -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let namespace = NamespaceSecret::new(&mut rng);
        let _replica = store.new_replica(namespace.clone())?;
        let author = store.new_author(&mut rng)?;
        store.close_replica(namespace.id());
        let mut replica = store.open_replica(&namespace.id())?;

        let key = b"hello";
        let value = b"world";
        let entry = {
            let timestamp = 2;
            let id = RecordIdentifier::new(namespace.id(), author.id(), key);
            let record = Record::from_data(value, timestamp);
            Entry::new(id, record).sign(&namespace, &author)
        };

        replica
            .insert_entry(entry.clone(), InsertOrigin::Local)
            .await
            .unwrap();
        store.close_replica(namespace.id());
        let res = store
            .get_exact(namespace.id(), author.id(), key, false)?
            .unwrap();
        assert_eq!(res, entry);

        let entry2 = {
            let timestamp = 1;
            let id = RecordIdentifier::new(namespace.id(), author.id(), key);
            let record = Record::from_data(value, timestamp);
            Entry::new(id, record).sign(&namespace, &author)
        };

        let mut replica = store.open_replica(&namespace.id())?;
        let res = replica.insert_entry(entry2, InsertOrigin::Local).await;
        store.close_replica(namespace.id());
        assert!(matches!(res, Err(InsertError::NewerEntryExists)));
        let res = store
            .get_exact(namespace.id(), author.id(), key, false)?
            .unwrap();
        assert_eq!(res, entry);
        store.flush()?;
        Ok(())
    }

    /// Two replicas each lacking entries of the other hold every entry of
    /// both after one session, Alice sending 2 entries and Bob 6.
    #[tokio::test]
    async fn test_replica_sync_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();

        test_replica_sync(alice_store, bob_store).await?;
        Ok(())
    }

    /// The fs-store twin of `test_replica_sync_memory`.
    #[tokio::test]
    async fn test_replica_sync_fs() -> Result<()> {
        let alice_dbfile = tempfile::NamedTempFile::new()?;
        let alice_store = store::fs::Store::persistent(alice_dbfile.path(), TEST_CACHE_BYTES)?;
        let bob_dbfile = tempfile::NamedTempFile::new()?;
        let bob_store = store::fs::Store::persistent(bob_dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_sync(alice_store, bob_store).await?;

        Ok(())
    }

    async fn test_replica_sync(mut alice_store: Store, mut bob_store: Store) -> Result<()> {
        let alice_set = ["ape", "eel", "fox", "gnu"];
        let bob_set = ["bee", "cat", "doe", "eel", "fox", "hog"];

        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let mut alice = alice_store.new_replica(myspace.clone())?;
        for el in &alice_set {
            alice.hash_and_insert(el, &author, el.as_bytes()).await?;
        }

        let mut bob = bob_store.new_replica(myspace.clone())?;
        for el in &bob_set {
            bob.hash_and_insert(el, &author, el.as_bytes()).await?;
        }

        let (alice_out, bob_out) = sync(&mut alice, &mut bob).await?;

        assert_eq!(alice_out.num_sent, 2);
        assert_eq!(bob_out.num_recv, 2);
        assert_eq!(alice_out.num_recv, 6);
        assert_eq!(bob_out.num_sent, 6);

        check_entries(&mut alice_store, &myspace.id(), &author, &alice_set)?;
        check_entries(&mut alice_store, &myspace.id(), &author, &bob_set)?;
        check_entries(&mut bob_store, &myspace.id(), &author, &alice_set)?;
        check_entries(&mut bob_store, &myspace.id(), &author, &bob_set)?;
        alice_store.flush()?;
        bob_store.flush()?;
        Ok(())
    }

    /// One author's writes at one key on two replicas converge on both, after
    /// a session, on the later write, whichever side made it.
    #[tokio::test]
    async fn test_replica_timestamp_sync_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();

        test_replica_timestamp_sync(alice_store, bob_store).await?;
        Ok(())
    }

    /// The fs-store twin of `test_replica_timestamp_sync_memory`.
    #[tokio::test]
    async fn test_replica_timestamp_sync_fs() -> Result<()> {
        let alice_dbfile = tempfile::NamedTempFile::new()?;
        let alice_store = store::fs::Store::persistent(alice_dbfile.path(), TEST_CACHE_BYTES)?;
        let bob_dbfile = tempfile::NamedTempFile::new()?;
        let bob_store = store::fs::Store::persistent(bob_dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_timestamp_sync(alice_store, bob_store).await?;

        Ok(())
    }

    async fn test_replica_timestamp_sync(
        mut alice_store: Store,
        mut bob_store: Store,
    ) -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut alice = alice_store.new_replica(namespace.clone())?;
        let mut bob = bob_store.new_replica(namespace.clone())?;

        let key = b"key";
        let alice_value = b"alice";
        let bob_value = b"bob";
        let _alice_hash = alice.hash_and_insert(key, &author, alice_value).await?;
        // system time increased - sync should overwrite
        let bob_hash = bob.hash_and_insert(key, &author, bob_value).await?;
        sync(&mut alice, &mut bob).await?;
        assert_eq!(
            get_content_hash(&mut alice_store, namespace.id(), author.id(), key)?,
            Some(bob_hash)
        );
        assert_eq!(
            get_content_hash(&mut bob_store, namespace.id(), author.id(), key)?,
            Some(bob_hash)
        );

        let mut alice = alice_store.new_replica(namespace.clone())?;
        let mut bob = bob_store.new_replica(namespace.clone())?;

        let alice_value_2 = b"alice2";
        // system time increased - sync should overwrite
        let _bob_hash_2 = bob.hash_and_insert(key, &author, bob_value).await?;
        let alice_hash_2 = alice.hash_and_insert(key, &author, alice_value_2).await?;
        sync(&mut alice, &mut bob).await?;
        assert_eq!(
            get_content_hash(&mut alice_store, namespace.id(), author.id(), key)?,
            Some(alice_hash_2)
        );
        assert_eq!(
            get_content_hash(&mut bob_store, namespace.id(), author.id(), key)?,
            Some(alice_hash_2)
        );
        alice_store.flush()?;
        bob_store.flush()?;
        Ok(())
    }

    /// A session serves the store as of session setup: a write landing
    /// mid-session is not served within the session and travels on the
    /// next one.
    ///
    /// The snapshot is put on the replica here rather than opened through
    /// a handle: the subject is what the store layer does with one, and the
    /// actor's part — looking the snapshot up and handing it over — is what
    /// `a_session_serves_the_view_frozen_at_its_start` covers.
    #[tokio::test]
    async fn test_session_snapshot_freezes_egress_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();
        test_session_snapshot_freezes_egress(alice_store, bob_store).await
    }

    /// The fs-store twin of `test_session_snapshot_freezes_egress_memory`.
    #[tokio::test]
    async fn test_session_snapshot_freezes_egress_fs() -> Result<()> {
        let alice_dbfile = tempfile::NamedTempFile::new()?;
        let alice_store = store::fs::Store::persistent(alice_dbfile.path(), TEST_CACHE_BYTES)?;
        let bob_dbfile = tempfile::NamedTempFile::new()?;
        let bob_store = store::fs::Store::persistent(bob_dbfile.path(), TEST_CACHE_BYTES)?;
        test_session_snapshot_freezes_egress(alice_store, bob_store).await
    }

    async fn test_session_snapshot_freezes_egress(
        mut alice_store: Store,
        mut bob_store: Store,
    ) -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let mut alice = alice_store.new_replica(myspace.clone())?;
        for el in ["ape", "bee", "cat"] {
            alice.hash_and_insert(el, &author, el.as_bytes()).await?;
        }
        drop(alice);

        // Session setup: freeze alice's egress view.
        let snapshot = alice_store.snapshot_owned()?;
        let mut alice = alice_store.new_replica(myspace.clone())?;
        alice.store.session_snapshot = Some(&snapshot);
        let mut bob = bob_store.new_replica(myspace.clone())?;

        let first = alice.sync_initial_message(None)?;

        // Lands after session setup: must not be served in this session.
        alice.hash_and_insert("dog", &author, b"dog").await?;

        let (alice_state, bob_state) = sync_from_initial(&mut alice, &mut bob, first, None).await?;
        drop(snapshot);

        // The mid-session write was not even transmitted.
        assert_eq!(alice_state.num_sent, 3);
        assert_eq!(bob_state.num_recv, 3);

        // The frozen view delivered the pre-session set only.
        assert_keys(
            &mut bob_store,
            myspace.id(),
            vec![b"ape".to_vec(), b"bee".to_vec(), b"cat".to_vec()],
        );

        // The next session (fresh snapshot) delivers the held-back write.
        let snapshot = alice_store.snapshot_owned()?;
        let mut alice = alice_store.new_replica(myspace.clone())?;
        alice.store.session_snapshot = Some(&snapshot);
        let mut bob = bob_store.new_replica(myspace.clone())?;
        let (alice_state, bob_state) = sync(&mut alice, &mut bob).await?;
        let _ = (alice_state, bob_state);
        drop(snapshot);
        assert_keys(
            &mut bob_store,
            myspace.id(),
            vec![
                b"ape".to_vec(),
                b"bee".to_vec(),
                b"cat".to_vec(),
                b"dog".to_vec(),
            ],
        );
        alice_store.flush()?;
        bob_store.flush()?;
        Ok(())
    }

    /// Ingest keeps reading the live store under a session snapshot: an
    /// older remote entry loses against a newer local write the snapshot
    /// predates. Judged against the snapshot instead, the older entry
    /// would overwrite the newer one.
    ///
    /// The entries carry hand-picked timestamps, which is what decides the
    /// comparison under test and what a handle's own stamping cannot supply,
    /// so both replicas are driven directly and the snapshot is put on one
    /// of them here.
    #[tokio::test]
    async fn test_session_snapshot_keeps_ingest_reads_live() -> Result<()> {
        let mut rng = rand::rng();
        let mut alice_store = store::Store::memory();
        let mut bob_store = store::Store::memory();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let key = b"claim";

        let t = system_time_now();
        let older = SignedEntry::from_parts(&myspace, &author, key, Record::from_data(b"older", t));
        let newer =
            SignedEntry::from_parts(&myspace, &author, key, Record::from_data(b"newer", t + 10));

        let mut bob = bob_store.new_replica(myspace.clone())?;
        bob.insert_entry(older.clone(), InsertOrigin::Local).await?;

        // Alice's snapshot predates her newer write: egress serves nothing
        // for the key, while the ingest comparison still sees the write.
        let snapshot = alice_store.snapshot_owned()?;
        let mut alice = alice_store.new_replica(myspace.clone())?;
        alice.store.session_snapshot = Some(&snapshot);
        let first = alice.sync_initial_message(None)?;
        alice
            .insert_entry(newer.clone(), InsertOrigin::Local)
            .await?;

        let (alice_state, bob_state) = sync_from_initial(&mut alice, &mut bob, first, None).await?;
        drop(snapshot);

        // Bob transmitted the older entry into alice's session; alice's
        // frozen egress served nothing.
        assert_eq!(bob_state.num_sent, 1);
        assert_eq!(alice_state.num_recv, 1);
        assert_eq!(alice_state.num_sent, 0);

        // The newer local write survived the older remote entry.
        assert_eq!(
            get_entry(&mut alice_store, myspace.id(), author.id(), key)?,
            newer
        );
        // The frozen egress did not serve the newer entry: bob holds the
        // older one until the next session.
        assert_eq!(
            get_entry(&mut bob_store, myspace.id(), author.id(), key)?,
            older
        );

        // The next session converges bob onto the newer entry.
        let snapshot = alice_store.snapshot_owned()?;
        let mut alice = alice_store.new_replica(myspace.clone())?;
        alice.store.session_snapshot = Some(&snapshot);
        let mut bob = bob_store.new_replica(myspace.clone())?;
        let (alice_state, bob_state) = sync(&mut alice, &mut bob).await?;
        let _ = (alice_state, bob_state);
        drop(snapshot);
        assert_eq!(
            get_entry(&mut bob_store, myspace.id(), author.id(), key)?,
            newer
        );
        alice_store.flush()?;
        bob_store.flush()?;
        Ok(())
    }

    /// A held snapshot's fingerprint does not drift under live writes,
    /// while the live view and a younger snapshot see them — snapshots
    /// and the live store read side by side.
    ///
    /// The subject is `StoreInstance` itself, which is why the instances
    /// below are built by hand: two snapshots and a live view of one store
    /// have to be readable within one test, and a session gives out one
    /// view at a time.
    #[tokio::test]
    async fn test_session_snapshot_fingerprint_stable_under_live_writes() -> Result<()> {
        use crate::{ranger::Store as _, store::fs::StoreInstance};

        let mut rng = rand::rng();
        let mut store = store::Store::memory();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let ns = myspace.id();
        let full_range =
            crate::ranger::Range::new(RecordIdentifier::default(), RecordIdentifier::default());

        let mut replica = store.new_replica(myspace.clone())?;
        for el in ["ape", "bee"] {
            replica.hash_and_insert(el, &author, el.as_bytes()).await?;
        }
        drop(replica);

        let snap_before = store.snapshot_owned()?;
        let fp_before = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snap_before);
            inst.get_fingerprint(&full_range)?
        };

        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert("cat", &author, b"cat").await?;
        drop(replica);

        let fp_frozen = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snap_before);
            inst.get_fingerprint(&full_range)?
        };
        assert_eq!(fp_before, fp_frozen);

        let snap_after = store.snapshot_owned()?;
        let fp_live = StoreInstance::new(ns, &mut store).get_fingerprint(&full_range)?;
        assert_ne!(fp_before, fp_live);
        let fp_younger = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snap_after);
            inst.get_fingerprint(&full_range)?
        };
        assert_eq!(fp_live, fp_younger);
        // The older snapshot still reads unchanged next to the younger one.
        let fp_older_again = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snap_before);
            inst.get_fingerprint(&full_range)?
        };
        assert_eq!(fp_before, fp_older_again);
        store.flush()?;
        Ok(())
    }

    /// A session opened while nothing was written since the last commit
    /// commits nothing, and one opened after a write commits it. Opening a
    /// session flushes, and reads open a write transaction of their own, so
    /// without the check every session would commit an empty transaction — a
    /// durable file sync on the one actor thread every replica shares.
    #[tokio::test]
    async fn a_session_over_an_unwritten_store_commits_nothing_memory() -> Result<()> {
        let store = store::Store::memory();
        a_session_over_an_unwritten_store_commits_nothing(store).await
    }

    /// The fs-store twin of `a_session_over_an_unwritten_store_commits_nothing_memory`.
    #[tokio::test]
    async fn a_session_over_an_unwritten_store_commits_nothing_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        a_session_over_an_unwritten_store_commits_nothing(store).await
    }

    async fn a_session_over_an_unwritten_store_commits_nothing(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let ns = myspace.id();

        // A write, then a session: the write is committed and served. At
        // least once, not exactly: a pause past `MAX_COMMIT_DELAY` between
        // the two writes commits the first transaction on its own.
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert("ape", &author, b"ape").await?;
        drop(replica);
        let snapshot = store.snapshot_owned()?;
        assert!(
            store.debug_commit_count() >= 1,
            "the write was never committed"
        );
        drop(snapshot);
        assert_keys(&mut store, ns, vec![b"ape".to_vec()]);

        // A read that goes through `tables` leaves a write transaction
        // open — that is what makes the clean commit the ordinary case —
        // and a session over it commits nothing.
        let commits_before = store.debug_commit_count();
        for _ in 0..5 {
            store.get_sync_peers(&ns)?;
            assert!(
                matches!(store.debug_transaction_kind(), "write"),
                "the read left no write transaction open, so the test proves nothing"
            );
            let snapshot = store.snapshot_owned()?;
            assert_eq!(store.debug_commit_count(), commits_before);
            drop(snapshot);
        }

        // A write makes the next session commit again, and the snapshot
        // serves it — the skip is of an empty commit, not of a real one.
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert("bee", &author, b"bee").await?;
        drop(replica);
        let snapshot = store.snapshot_owned()?;
        assert!(
            store.debug_commit_count() > commits_before,
            "a real write was skipped as if the transaction were clean"
        );
        drop(snapshot);
        assert_keys(&mut store, ns, vec![b"ape".to_vec(), b"bee".to_vec()]);
        store.flush()?;
        Ok(())
    }

    /// Opening a session commits the store's pending write batch
    /// (`snapshot_owned` commits it): an entry inserted right before session
    /// setup is in the snapshot, and the store stays writable afterwards
    /// with the new batch invisible to the held snapshot.
    ///
    /// The subject is the store's own flush behaviour, so the snapshot is
    /// taken and put on a `StoreInstance` here — no exchange is involved.
    #[tokio::test]
    async fn test_session_snapshot_commits_pending_write_batch_memory() -> Result<()> {
        let store = store::Store::memory();
        test_session_snapshot_commits_pending_write_batch(store).await
    }

    /// The fs-store twin of `test_session_snapshot_commits_pending_write_batch_memory`.
    #[tokio::test]
    async fn test_session_snapshot_commits_pending_write_batch_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_session_snapshot_commits_pending_write_batch(store).await
    }

    async fn test_session_snapshot_commits_pending_write_batch(mut store: Store) -> Result<()> {
        use crate::{ranger::Store as _, store::fs::StoreInstance};

        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let ns = myspace.id();
        let full_range =
            crate::ranger::Range::new(RecordIdentifier::default(), RecordIdentifier::default());

        // Sits in the open write batch: nothing flushed it yet.
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert("ape", &author, b"ape").await?;
        drop(replica);

        let snapshot = store.snapshot_owned()?;
        let keys: Vec<_> = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snapshot);
            inst.get_range(full_range.clone())?
                .map(|e| e.map(|e| e.key().to_vec()))
                .collect::<Result<_, _>>()?
        };
        assert_eq!(keys, vec![b"ape".to_vec()]);

        // The forced commit leaves the store writable; the next batch is
        // invisible to the held snapshot and visible live.
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert("bee", &author, b"bee").await?;
        drop(replica);
        let keys_frozen: Vec<_> = {
            let mut inst = StoreInstance::new(ns, &mut store);
            inst.session_snapshot = Some(&snapshot);
            inst.get_range(full_range.clone())?
                .map(|e| e.map(|e| e.key().to_vec()))
                .collect::<Result<_, _>>()?
        };
        assert_eq!(keys_frozen, vec![b"ape".to_vec()]);
        assert_keys(&mut store, ns, vec![b"ape".to_vec(), b"bee".to_vec()]);
        store.flush()?;
        Ok(())
    }

    /// Two namespaces in one store, with entries in both. Returns the store,
    /// the namespace under exchange, the neighbour, and the shared author.
    async fn store_with_two_namespaces() -> Result<(Store, NamespaceSecret, NamespaceSecret, Author)>
    {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(7);
        let mut store = store::Store::memory();
        let author = Author::new(&mut rng);
        let (mine, neighbour) = {
            let a = NamespaceSecret::new(&mut rng);
            let b = NamespaceSecret::new(&mut rng);
            // Order them, so the neighbour's rows sit on a known side of
            // mine in the shared records table.
            if a.id() < b.id() {
                (a, b)
            } else {
                (b, a)
            }
        };

        let mut replica = store.new_replica(mine.clone())?;
        replica.hash_and_insert("mine", &author, b"mine").await?;
        drop(replica);
        let mut replica = store.new_replica(neighbour.clone())?;
        for key in ["secret-a", "secret-b"] {
            replica
                .hash_and_insert(key, &author, key.as_bytes())
                .await?;
        }
        drop(replica);

        Ok((store, mine, neighbour, author))
    }

    /// A range boundary is checked against the namespace under exchange.
    /// Every namespace's records share one table, so an unchecked boundary
    /// reads the neighbour's rows — and reconciliation hands what it reads
    /// to the peer.
    #[tokio::test]
    async fn a_sync_range_naming_another_namespace_is_refused() -> Result<()> {
        use crate::{ranger::Store as _, store::fs::StoreInstance};

        let (mut store, mine, neighbour, author) = store_with_two_namespaces().await?;
        let id = mine.id();
        let all = Range::new(RecordIdentifier::default(), RecordIdentifier::default());
        let at =
            |ns: &NamespaceSecret, key: &[u8]| RecordIdentifier::new(ns.id(), author.id(), key);

        // The neighbour's rows are really there, so a refusal below is a
        // refusal to read them rather than an empty range.
        assert_eq!(
            store
                .get_many(neighbour.id(), Query::all())?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            2
        );

        // Authorized: the identity range and a boundary of this namespace
        // both serve, and serve only this namespace's rows.
        let mut inst = StoreInstance::new(id, &mut store);
        let served: Vec<_> = inst.get_range(all.clone())?.collect::<Result<_, _>>()?;
        assert_eq!(served.len(), 1);
        assert!(served.iter().all(|entry| entry.namespace() == id));
        assert!(inst
            .get_range(Range::new(at(&mine, b""), at(&mine, b"\xff")))
            .is_ok());
        assert!(inst.get_fingerprint(&all).is_ok());

        // Refused: a boundary naming the neighbour, in either range form,
        // and whether both boundaries are foreign or only one.
        let foreign_forward = Range::new(at(&neighbour, b""), at(&neighbour, b"\xff"));
        let foreign_wrapping = Range::new(at(&neighbour, b"\xff"), at(&neighbour, b""));
        let one_foreign_boundary = Range::new(at(&mine, b""), at(&neighbour, b"\xff"));
        for range in [&foreign_forward, &foreign_wrapping, &one_foreign_boundary] {
            assert!(
                inst.get_range(range.clone()).is_err(),
                "a foreign boundary served a range"
            );
            assert!(
                inst.get_fingerprint(range).is_err(),
                "a foreign boundary served a fingerprint"
            );
        }
        Ok(())
    }

    /// An identifier naming another namespace is refused by the ingest read,
    /// the prefix read and the write, so a store instance is safe without
    /// `validate_entry` in front of it. The write is probed directly as well
    /// as through `put`, since `put` reads before it writes and the read
    /// alone would refuse. The neighbour's rows are read back at the end: a
    /// refusal has to leave them as they were.
    #[tokio::test]
    async fn an_identifier_naming_another_namespace_is_refused() -> Result<()> {
        use crate::{ranger::Store as _, store::fs::StoreInstance};

        let (mut store, mine, neighbour, author) = store_with_two_namespaces().await?;
        let at =
            |ns: &NamespaceSecret, key: &[u8]| RecordIdentifier::new(ns.id(), author.id(), key);
        let rows = |store: &mut Store, ns: &NamespaceSecret| -> Result<Vec<SignedEntry>> {
            store.get_many(ns.id(), Query::all())?.collect()
        };
        let before = rows(&mut store, &neighbour)?;
        assert_eq!(before.len(), 2);

        let mut inst = StoreInstance::new(mine.id(), &mut store);

        // Allowed: this namespace's identifier reads its own rows.
        assert!(inst.entry_get(&at(&mine, b"mine"))?.is_some());

        // Denied: the neighbour's identifier, on every keyed read.
        assert!(
            inst.entry_get(&at(&neighbour, b"secret-a")).is_err(),
            "a foreign identifier read the neighbour's entry"
        );
        assert!(
            inst.prefixed_by(&at(&neighbour, b"secret")).is_err(),
            "a foreign identifier read the neighbour's prefix"
        );

        // Denied: a well-signed neighbour entry through `put` itself.
        let entry = Entry::new(
            at(&neighbour, b"secret-a"),
            Record::current_from_data(b"forged"),
        );
        let entry = SignedEntry::from_entry(entry, &neighbour, &author);
        assert!(
            inst.put(entry.clone()).is_err(),
            "a foreign entry was taken in"
        );
        assert!(
            inst.entry_put(entry).is_err(),
            "a foreign entry was written past the read"
        );

        // Allowed: this namespace's entry through `put`, so the denial above
        // is a refusal rather than a `put` that takes nothing.
        let entry = Entry::new(at(&mine, b"mine"), Record::current_from_data(b"newer"));
        let entry = SignedEntry::from_entry(entry, &mine, &author);
        assert!(matches!(
            inst.put(entry)?,
            crate::ranger::InsertOutcome::Inserted { removed: 1 }
        ));

        assert_eq!(rows(&mut store, &neighbour)?, before);
        Ok(())
    }

    /// The same check on the wire: a peer with a session on one namespace
    /// crafts a range over the neighbour's rows. An empty fingerprint puts
    /// the reply straight into the recursion anchor, which transmits every
    /// entry of the range.
    #[tokio::test]
    async fn a_crafted_range_over_another_namespace_serves_nothing() -> Result<()> {
        use crate::ranger::{Fingerprint, MessagePart, RangeFingerprint};

        let (mut store, mine, neighbour, author) = store_with_two_namespaces().await?;
        let mut replica = store.new_replica(mine.clone())?;

        let range = Range::new(
            RecordIdentifier::new(neighbour.id(), author.id(), b""),
            RecordIdentifier::new(neighbour.id(), author.id(), b"\xff"),
        );
        let crafted = crate::ranger::Message::from_parts(vec![MessagePart::RangeFingerprint(
            RangeFingerprint {
                range,
                fingerprint: Fingerprint::empty(),
            },
        )]);

        let mut state = SyncOutcome::default();
        let reply = replica
            .sync_process_message(crafted, [9u8; 32], &mut state, None, None)
            .await;
        assert!(reply.is_err(), "the crafted range was served");
        assert_eq!(state.num_sent, 0);
        drop(replica);

        // The neighbour's replica is untouched and still readable through
        // its own namespace — the refusal is of the caller, not of the data.
        assert_keys(
            &mut store,
            neighbour.id(),
            vec![b"secret-a".to_vec(), b"secret-b".to_vec()],
        );
        Ok(())
    }

    /// A local write stamped up to `MAX_TIMESTAMP_FUTURE_SHIFT` ahead of the
    /// clock is taken, and one stamped past it is refused as
    /// `TooFarInTheFuture`, the entry before it standing.
    #[tokio::test]
    async fn test_future_timestamp() -> Result<()> {
        let mut rng = rand::rng();
        let mut store = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);

        let mut replica = store.new_replica(namespace.clone())?;
        let key = b"hi";
        let t = system_time_now();
        let record = Record::from_data(b"1", t);
        let entry0 = SignedEntry::from_parts(&namespace, &author, key, record);
        replica
            .insert_entry(entry0.clone(), InsertOrigin::Local)
            .await?;

        assert_eq!(
            get_entry(&mut store, namespace.id(), author.id(), key)?,
            entry0
        );

        let mut replica = store.new_replica(namespace.clone())?;
        let t = system_time_now() + MAX_TIMESTAMP_FUTURE_SHIFT - 10000;
        let record = Record::from_data(b"2", t);
        let entry1 = SignedEntry::from_parts(&namespace, &author, key, record);
        replica
            .insert_entry(entry1.clone(), InsertOrigin::Local)
            .await?;
        assert_eq!(
            get_entry(&mut store, namespace.id(), author.id(), key)?,
            entry1
        );

        let mut replica = store.new_replica(namespace.clone())?;
        let t = system_time_now() + MAX_TIMESTAMP_FUTURE_SHIFT;
        let record = Record::from_data(b"2", t);
        let entry2 = SignedEntry::from_parts(&namespace, &author, key, record);
        replica
            .insert_entry(entry2.clone(), InsertOrigin::Local)
            .await?;
        assert_eq!(
            get_entry(&mut store, namespace.id(), author.id(), key)?,
            entry2
        );

        let mut replica = store.new_replica(namespace.clone())?;
        let t = system_time_now() + MAX_TIMESTAMP_FUTURE_SHIFT + 10000;
        let record = Record::from_data(b"2", t);
        let entry3 = SignedEntry::from_parts(&namespace, &author, key, record);
        let res = replica.insert_entry(entry3, InsertOrigin::Local).await;
        assert!(matches!(
            res,
            Err(InsertError::Validation(
                ValidationFailure::TooFarInTheFuture
            ))
        ));
        assert_eq!(
            get_entry(&mut store, namespace.id(), author.id(), key)?,
            entry2
        );
        store.flush()?;
        Ok(())
    }

    /// Regression: `MAX_TIMESTAMP_FUTURE_SHIFT` must be on the same unit scale as
    /// `system_time_now()` (Unix epoch **microseconds**). A millisecond-scaled constant
    /// only allows ~0.6s of future slack and would reject this insert.
    #[tokio::test]
    async fn test_future_timestamp_accepts_one_second_skew() -> Result<()> {
        let mut rng = rand::rng();
        let mut store = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(namespace.clone())?;
        let key = b"skew";
        let skew_micros: u64 = Duration::from_secs(1).as_micros() as u64;
        let now = system_time_now();
        let record = Record::from_data(b"ahead", now + skew_micros);
        let entry = SignedEntry::from_parts(&namespace, &author, key, record);
        replica
            .insert_entry(entry.clone(), InsertOrigin::Local)
            .await?;
        assert_eq!(
            get_entry(&mut store, namespace.id(), author.id(), key)?,
            entry
        );
        store.flush()?;
        Ok(())
    }

    /// A local write at a key longer than `MAX_KEY_BYTES` is refused, and
    /// one at exactly the bound is written.
    #[tokio::test]
    async fn a_key_over_the_bound_is_refused_on_a_local_write() -> Result<()> {
        let mut rng = rand::rng();
        let mut store = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(namespace.clone())?;

        let at_bound = vec![b'k'; MAX_KEY_BYTES];
        replica.hash_and_insert(&at_bound, &author, "v").await?;
        let over = vec![b'k'; MAX_KEY_BYTES + 1];
        let res = replica.hash_and_insert(&over, &author, "v").await;
        assert!(
            matches!(
                res,
                Err(InsertError::Validation(ValidationFailure::KeyTooLong))
            ),
            "a key over the bound was written: {res:?}"
        );
        drop(replica);
        assert!(get_entry(&mut store, namespace.id(), author.id(), &at_bound).is_ok());
        Ok(())
    }

    /// A local write of empty content is refused as `EntryIsEmpty`.
    #[tokio::test]
    async fn test_insert_empty() -> Result<()> {
        let mut store = store::Store::memory();
        let mut rng = rand::rng();
        let alice = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(myspace.clone())?;
        let hash = Hash::new(b"");
        let res = replica.insert(b"foo", &alice, hash, 0).await;
        assert!(matches!(res, Err(InsertError::EntryIsEmpty)));
        store.flush()?;
        Ok(())
    }

    /// A delete at a key removes the entry there and leaves every longer
    /// key it prefixes standing.
    #[tokio::test]
    async fn test_delete_touches_only_its_key_memory() -> Result<()> {
        let store = store::Store::memory();
        test_delete_touches_only_its_key(store).await?;
        Ok(())
    }

    /// The fs-store twin of `test_delete_touches_only_its_key_memory`.
    #[tokio::test]
    async fn test_delete_touches_only_its_key_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_delete_touches_only_its_key(store).await?;
        Ok(())
    }

    async fn test_delete_touches_only_its_key(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let alice = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert(b"foo", &alice, b"parent").await?;
        let hash1 = replica.hash_and_insert(b"foobar", &alice, b"hello").await?;
        let hash2 = replica
            .hash_and_insert(b"foo/bar", &alice, b"world")
            .await?;

        let mut replica = store.new_replica(myspace.clone())?;
        let deleted = replica.delete(b"foo", &alice).await?;
        assert_eq!(deleted, 1);
        assert_eq!(
            store.get_exact(myspace.id(), alice.id(), b"foo", false)?,
            None
        );
        assert_eq!(
            get_content_hash(&mut store, myspace.id(), alice.id(), b"foobar")?,
            Some(hash1)
        );
        assert_eq!(
            get_content_hash(&mut store, myspace.id(), alice.id(), b"foo/bar")?,
            Some(hash2)
        );

        // A write at a key likewise leaves the longer keys it prefixes.
        let mut replica = store.new_replica(myspace.clone())?;
        replica.hash_and_insert(b"foo", &alice, b"again").await?;
        assert_eq!(
            get_content_hash(&mut store, myspace.id(), alice.id(), b"foobar")?,
            Some(hash1)
        );
        store.flush()?;
        Ok(())
    }

    /// A delete at one key reaches the peer over a session, and the entries
    /// at the longer keys it prefixes stand on both replicas.
    #[tokio::test]
    async fn test_replica_sync_delete_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        let bob_store = store::Store::memory();

        test_replica_sync_delete(alice_store, bob_store).await
    }

    /// The fs-store twin of `test_replica_sync_delete_memory`.
    #[tokio::test]
    async fn test_replica_sync_delete_fs() -> Result<()> {
        let alice_dbfile = tempfile::NamedTempFile::new()?;
        let alice_store = store::fs::Store::persistent(alice_dbfile.path(), TEST_CACHE_BYTES)?;
        let bob_dbfile = tempfile::NamedTempFile::new()?;
        let bob_store = store::fs::Store::persistent(bob_dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_sync_delete(alice_store, bob_store).await
    }

    async fn test_replica_sync_delete(mut alice_store: Store, mut bob_store: Store) -> Result<()> {
        let alice_set = ["foot"];
        let bob_set = ["fool", "foo", "fog"];

        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let myspace = NamespaceSecret::new(&mut rng);
        let mut alice = alice_store.new_replica(myspace.clone())?;
        for el in &alice_set {
            alice.hash_and_insert(el, &author, el.as_bytes()).await?;
        }

        let mut bob = bob_store.new_replica(myspace.clone())?;
        for el in &bob_set {
            bob.hash_and_insert(el, &author, el.as_bytes()).await?;
        }

        sync(&mut alice, &mut bob).await?;

        let keys = |set: &[&str]| set.iter().map(|el| el.as_bytes().to_vec()).collect();
        let synced = ["foot", "fool", "foo", "fog"];
        for store in [&mut alice_store, &mut bob_store] {
            check_entries(store, &myspace.id(), &author, &synced)?;
            assert_keys(store, myspace.id(), keys(&synced));
        }

        let mut alice = alice_store.new_replica(myspace.clone())?;
        let mut bob = bob_store.new_replica(myspace.clone())?;
        alice.delete("foo", &author).await?;
        bob.hash_and_insert("fooz", &author, "fooz".as_bytes())
            .await?;
        sync(&mut alice, &mut bob).await?;
        // Exact: a longer key gone missing fails it as the deleted one come back does.
        let standing = ["foot", "fool", "fog", "fooz"];
        for store in [&mut alice_store, &mut bob_store] {
            check_entries(store, &myspace.id(), &author, &standing)?;
            assert_keys(store, myspace.id(), keys(&standing));
        }
        alice_store.flush()?;
        bob_store.flush()?;
        Ok(())
    }

    /// A replica is removed only once closed, takes its entries with it and
    /// cannot be reopened, and its namespace can then be created again.
    #[tokio::test]
    async fn test_replica_remove_memory() -> Result<()> {
        let alice_store = store::Store::memory();
        test_replica_remove(alice_store).await
    }

    /// The fs-store twin of `test_replica_remove_memory`.
    #[tokio::test]
    async fn test_replica_remove_fs() -> Result<()> {
        let alice_dbfile = tempfile::NamedTempFile::new()?;
        let alice_store = store::fs::Store::persistent(alice_dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_remove(alice_store).await
    }

    async fn test_replica_remove(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);
        let author = Author::new(&mut rng);
        let mut replica = store.new_replica(namespace.clone())?;

        // insert entry
        let hash = replica.hash_and_insert(b"foo", &author, b"bar").await?;
        let res = store
            .get_many(namespace.id(), Query::all())?
            .collect::<Vec<_>>();
        assert_eq!(res.len(), 1);

        // remove replica
        let res = store.remove_replica(&namespace.id());
        // may not remove replica while still open;
        assert!(res.is_err());
        store.close_replica(namespace.id());
        store.remove_replica(&namespace.id())?;
        let res = store
            .get_many(namespace.id(), Query::all())?
            .collect::<Vec<_>>();
        assert_eq!(res.len(), 0);

        // may not reopen removed replica
        let res = store.load_replica_info(&namespace.id());
        assert!(matches!(res, Err(OpenError::NotFound)));

        // may recreate replica
        let mut replica = store.new_replica(namespace.clone())?;
        replica.insert(b"foo", &author, hash, 3).await?;
        let res = store
            .get_many(namespace.id(), Query::all())?
            .collect::<Vec<_>>();
        assert_eq!(res.len(), 1);
        store.flush()?;
        Ok(())
    }

    /// A delete removes only its own key at the byte values 0 and 255 too:
    /// the two-byte keys a one-byte key prefixes stand, and a delete between
    /// two neighbours leaves both.
    #[tokio::test]
    async fn test_replica_delete_edge_cases_memory() -> Result<()> {
        let store = store::Store::memory();
        test_replica_delete_edge_cases(store).await
    }

    /// The fs-store twin of `test_replica_delete_edge_cases_memory`.
    #[tokio::test]
    async fn test_replica_delete_edge_cases_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_delete_edge_cases(store).await
    }

    async fn test_replica_delete_edge_cases(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);

        let edgecases = [0u8, 1u8, 255u8];
        let prefixes = [0u8, 255u8];
        let hash = Hash::new(b"foo");
        let len = 3;
        let mut expected = vec![];
        for prefix in prefixes {
            let mut replica = store.new_replica(namespace.clone())?;
            replica.insert([prefix], &author, hash, len).await?;
            for suffix in edgecases {
                let key = [prefix, suffix].to_vec();
                expected.push(key.clone());
                replica.insert(&key, &author, hash, len).await?;
            }
            let mut replica = store.new_replica(namespace.clone())?;
            replica.delete([prefix], &author).await?;
            assert_keys(&mut store, namespace.id(), expected.clone());
        }

        let mut replica = store.new_replica(namespace.clone())?;
        for key in [[1u8, 0u8], [1u8, 1u8], [1u8, 2u8]] {
            replica.insert(key, &author, hash, len).await?;
        }
        replica.delete([1u8, 1u8], &author).await?;
        expected.extend([vec![1u8, 0u8], vec![1u8, 2u8]]);
        assert_keys(&mut store, namespace.id(), expected);
        store.flush()?;
        Ok(())
    }

    /// The latest entry per author is each author's last write, a second
    /// author's counted apart.
    #[tokio::test]
    async fn test_latest_iter_memory() -> Result<()> {
        let store = store::Store::memory();
        test_latest_iter(store).await
    }

    /// The fs-store twin of `test_latest_iter_memory`.
    #[tokio::test]
    async fn test_latest_iter_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_latest_iter(store).await
    }

    /// An entry that arrives out of order leaves the author's head where it
    /// was: the head is the greatest timestamp the replica holds, not the
    /// one written last. A head that moved backwards would make this
    /// replica report less than it holds, and a peer comparing against it
    /// would stop offering what it has.
    #[tokio::test]
    async fn an_older_entry_does_not_move_the_author_head_back() -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut store = store::fs::Store::memory();
        let mut replica = store.new_replica(namespace.clone())?;

        let signed = |key: &[u8], timestamp: u64| {
            let id = RecordIdentifier::new(namespace.id(), author.id(), key);
            let record = Record::from_data(b"hi", timestamp);
            Entry::new(id, record).sign(&namespace, &author)
        };
        replica
            .insert_entry(signed(b"newer", 20), InsertOrigin::Local)
            .await?;
        replica
            .insert_entry(signed(b"older", 10), InsertOrigin::Local)
            .await?;
        store.close_replica(namespace.id());

        let latest = store
            .get_latest_for_each_author(namespace.id())?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(latest.len(), 1, "one author, one head");
        assert_eq!(
            (latest[0].1, latest[0].2.as_slice()),
            (20, b"newer".as_ref()),
            "the older entry took the head"
        );
        Ok(())
    }

    async fn test_latest_iter(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let author0 = Author::new(&mut rng);
        let author1 = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut replica = store.new_replica(namespace.clone())?;

        replica.hash_and_insert(b"a0.1", &author0, b"hi").await?;
        let latest = store
            .get_latest_for_each_author(namespace.id())?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].2, b"a0.1".to_vec());

        let mut replica = store.new_replica(namespace.clone())?;
        replica.hash_and_insert(b"a1.1", &author1, b"hi").await?;
        replica.hash_and_insert(b"a0.2", &author0, b"hi").await?;
        let latest = store
            .get_latest_for_each_author(namespace.id())?
            .collect::<Result<Vec<_>>>()?;
        let mut latest_keys: Vec<Vec<u8>> = latest.iter().map(|r| r.2.to_vec()).collect();
        latest_keys.sort();
        assert_eq!(latest_keys, vec![b"a0.2".to_vec(), b"a1.1".to_vec()]);
        store.flush()?;
        Ok(())
    }

    /// Binary keys, the bytes 0 and 255 among them, each stand as a key of
    /// their own.
    #[tokio::test]
    async fn test_replica_byte_keys_memory() -> Result<()> {
        let store = store::Store::memory();

        test_replica_byte_keys(store).await?;
        Ok(())
    }

    /// The fs-store twin of `test_replica_byte_keys_memory`.
    #[tokio::test]
    async fn test_replica_byte_keys_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_byte_keys(store).await?;

        Ok(())
    }

    async fn test_replica_byte_keys(mut store: Store) -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);

        let hash = Hash::new(b"foo");
        let len = 3;

        let key = vec![1u8, 0u8];
        let mut replica = store.new_replica(namespace.clone())?;
        replica.insert(key, &author, hash, len).await?;
        assert_keys(&mut store, namespace.id(), vec![vec![1u8, 0u8]]);
        let key = vec![1u8, 2u8];
        let mut replica = store.new_replica(namespace.clone())?;
        replica.insert(key, &author, hash, len).await?;
        assert_keys(
            &mut store,
            namespace.id(),
            vec![vec![1u8, 0u8], vec![1u8, 2u8]],
        );

        let key = vec![0u8, 255u8];
        let mut replica = store.new_replica(namespace.clone())?;
        replica.insert(key, &author, hash, len).await?;
        assert_keys(
            &mut store,
            namespace.id(),
            vec![vec![1u8, 0u8], vec![1u8, 2u8], vec![0u8, 255u8]],
        );
        store.flush()?;
        Ok(())
    }

    /// A dropped entry is offered again: the session's ingest verdict keeps
    /// it out of the receiving store, the sender keeps its copy, and the
    /// sender's next session delivers it once no verdict drops it.
    #[tokio::test]
    async fn a_dropped_entry_is_offered_again() -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let mut store1 = store::Store::memory();
        let mut store2 = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let id = namespace.id();

        let mut alice = store1.new_replica(namespace.clone())?;
        alice
            .hash_and_insert("dropped", &author, b"payload")
            .await?;
        alice.hash_and_insert("ok", &author, b"payload").await?;
        let mut bob = store2.new_replica(namespace.clone())?;

        let drop_one: crate::filter::SessionIngest = Arc::new(|entry: &SignedEntry| {
            if entry.key() == b"dropped" {
                ValidateOutcome::Drop
            } else {
                ValidateOutcome::Accept
            }
        });
        let first = alice.sync_initial_message(None)?;
        sync_transcript(&mut alice, &mut bob, first, None, Some(drop_one)).await?;
        assert!(store2.get_exact(id, author.id(), b"ok", false)?.is_some());
        assert!(store2
            .get_exact(id, author.id(), b"dropped", false)?
            .is_none());
        assert!(store1
            .get_exact(id, author.id(), b"dropped", false)?
            .is_some());

        let mut alice = store1.open_replica(&id)?;
        let mut bob = store2.open_replica(&id)?;
        sync(&mut alice, &mut bob).await?;
        assert!(
            store2
                .get_exact(id, author.id(), b"dropped", false)?
                .is_some(),
            "the next session must offer the dropped entry again"
        );
        Ok(())
    }

    /// A session whose egress filter admits one of three entries delivers
    /// that entry alone.
    #[tokio::test]
    async fn an_egress_predicate_delivers_only_what_it_admits() -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let mut alice_store = store::Store::memory();
        let mut bob_store = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);

        let mut bob = bob_store.new_replica(namespace.clone())?;
        for key in ["ape", "bee", "cat"] {
            bob.hash_and_insert(key, &author, key.as_bytes()).await?;
        }
        let mut alice = alice_store.new_replica(namespace.clone())?;
        let only_bee: crate::filter::EntryFilter =
            Arc::new(|entry: &SignedEntry| entry.key() == b"bee");

        let (_, bob_state) = sync_filtered(&mut alice, &mut bob, Some(only_bee)).await?;

        assert_eq!(bob_state.num_sent, 1);
        assert_keys(&mut alice_store, namespace.id(), vec![b"bee".to_vec()]);
        Ok(())
    }

    /// An entry the egress predicate withholds between two it admits leaves
    /// the session's transcript exactly as it is without that entry: no
    /// fingerprint, range boundary or item count betrays it.
    #[tokio::test]
    async fn a_withheld_entry_leaves_the_transcript_of_its_absence() -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let t = system_time_now();
        let entry = |key: &str| {
            SignedEntry::from_parts(
                &namespace,
                &author,
                key.as_bytes(),
                Record::from_data(key.as_bytes(), t),
            )
        };
        let hide_bee: crate::filter::EntryFilter =
            Arc::new(|entry: &SignedEntry| entry.key() != b"bee");

        let mut transcripts = Vec::new();
        for (held, egress) in [
            (&["ape", "bee", "cat"][..], Some(hide_bee)),
            (&["ape", "cat"][..], None),
        ] {
            let mut alice_store = store::Store::memory();
            let mut bob_store = store::Store::memory();
            let mut alice = alice_store.new_replica(namespace.clone())?;
            for key in ["ape", "dog"] {
                alice.insert_entry(entry(key), InsertOrigin::Local).await?;
            }
            let mut bob = bob_store.new_replica(namespace.clone())?;
            for key in held {
                bob.insert_entry(entry(key), InsertOrigin::Local).await?;
            }
            let first = alice.sync_initial_message(None)?;
            let (_, _, transcript) =
                sync_transcript(&mut alice, &mut bob, first, egress, None).await?;
            transcripts.push(transcript);
        }

        assert!(transcripts[0].len() > 2, "the session ran several rounds");
        assert_eq!(transcripts[0], transcripts[1]);
        Ok(())
    }

    /// An entry the session's ingest verdict drops stays out of the store,
    /// and the egress of the same session serves what it admits and nothing
    /// more.
    #[tokio::test]
    async fn an_ingest_drop_leaves_egress_unchanged() -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let mut alice_store = store::Store::memory();
        let mut bob_store = store::Store::memory();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);

        let mut alice = alice_store.new_replica(namespace.clone())?;
        for key in ["dog", "eel"] {
            alice.hash_and_insert(key, &author, key.as_bytes()).await?;
        }
        let mut bob = bob_store.new_replica(namespace.clone())?;
        for key in ["ape", "bee", "cat"] {
            bob.hash_and_insert(key, &author, key.as_bytes()).await?;
        }
        let hide_bee: crate::filter::EntryFilter =
            Arc::new(|entry: &SignedEntry| entry.key() != b"bee");
        let drop_dog: crate::filter::SessionIngest = Arc::new(|entry: &SignedEntry| {
            if entry.key() == b"dog" {
                ValidateOutcome::Drop
            } else {
                ValidateOutcome::Accept
            }
        });

        let first = alice.sync_initial_message(None)?;
        sync_transcript(&mut alice, &mut bob, first, Some(hide_bee), Some(drop_dog)).await?;

        assert_keys(
            &mut bob_store,
            namespace.id(),
            vec![
                b"ape".to_vec(),
                b"bee".to_vec(),
                b"cat".to_vec(),
                b"eel".to_vec(),
            ],
        );
        assert_keys(
            &mut alice_store,
            namespace.id(),
            vec![
                b"ape".to_vec(),
                b"cat".to_vec(),
                b"dog".to_vec(),
                b"eel".to_vec(),
            ],
        );
        Ok(())
    }

    fn drain(events: async_channel::Receiver<Event>) -> Vec<Event> {
        let mut res = vec![];
        while let Ok(ev) = events.try_recv() {
            res.push(ev);
        }
        res
    }

    /// This tests that no events are emitted for entries received during sync which are obsolete
    /// (too old) by the time they are actually inserted in the store.
    #[tokio::test]
    async fn test_replica_no_wrong_remote_insert_events() -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let mut store1 = store::Store::memory();
        let mut store2 = store::Store::memory();
        let peer1 = [1u8; 32];
        let peer2 = [2u8; 32];
        let mut state1 = SyncOutcome::default();
        let mut state2 = SyncOutcome::default();

        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let mut replica1 = store1.new_replica(namespace.clone())?;
        let mut replica2 = store2.new_replica(namespace.clone())?;

        let (events1_sender, events1) = async_channel::bounded(32);
        let (events2_sender, events2) = async_channel::bounded(32);

        replica1.info.subscribe(events1_sender);
        replica2.info.subscribe(events2_sender);

        replica1.hash_and_insert(b"foo", &author, b"init").await?;

        let from1 = replica1.sync_initial_message(None)?;
        let from2 = replica2
            .sync_process_message(from1, peer1, &mut state2, None, None)
            .await
            .unwrap()
            .unwrap();
        let from1 = replica1
            .sync_process_message(from2, peer2, &mut state1, None, None)
            .await
            .unwrap()
            .unwrap();
        // now we will receive the entry from rpelica1. we will insert a newer entry now, while the
        // sync is already running. this means the entry from replica1 will be rejected. we make
        // sure that no InsertRemote event is emitted for this entry.
        replica2.hash_and_insert(b"foo", &author, b"update").await?;
        let from2 = replica2
            .sync_process_message(from1, peer1, &mut state2, None, None)
            .await
            .unwrap();
        assert!(from2.is_none());
        let events1 = drain(events1);
        let events2 = drain(events2);
        assert_eq!(events1.len(), 1);
        assert_eq!(events2.len(), 1);
        assert!(matches!(events1[0], Event::LocalInsert { .. }));
        assert!(matches!(events2[0], Event::LocalInsert { .. }));
        assert_eq!(state1.num_sent, 1);
        assert_eq!(state1.num_recv, 0);
        assert_eq!(state2.num_sent, 0);
        assert_eq!(state2.num_recv, 1);
        store1.flush()?;
        store2.flush()?;
        Ok(())
    }

    /// Queries over one replica read every entry by author and then key, the
    /// latest per key, a prefix and a limit, and an entry its author deleted
    /// only under `include_empty`.
    #[tokio::test]
    async fn test_replica_queries_mem() -> Result<()> {
        let store = store::Store::memory();

        test_replica_queries(store).await?;
        Ok(())
    }

    /// The fs-store twin of `test_replica_queries_mem`.
    #[tokio::test]
    async fn test_replica_queries_fs() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let store = store::fs::Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;
        test_replica_queries(store).await?;

        Ok(())
    }

    async fn test_replica_queries(mut store: Store) -> Result<()> {
        let mut rng = rand::rngs::ChaCha12Rng::seed_from_u64(1);
        let namespace = NamespaceSecret::new(&mut rng);
        let namespace_id = namespace.id();

        let a1 = store.new_author(&mut rng)?;
        let a2 = store.new_author(&mut rng)?;
        let a3 = store.new_author(&mut rng)?;
        println!(
            "a1 {} a2 {} a3 {}",
            a1.id().fmt_short(),
            a2.id().fmt_short(),
            a3.id().fmt_short()
        );

        let mut replica = store.new_replica(namespace.clone())?;
        replica.hash_and_insert("hi/world", &a2, "a2").await?;
        replica.hash_and_insert("hi/world", &a1, "a1").await?;
        replica.hash_and_insert("hi/moon", &a2, "a1").await?;
        replica.hash_and_insert("hi", &a3, "a3").await?;

        struct QueryTester<'a> {
            store: &'a mut Store,
            namespace: NamespaceId,
        }
        impl QueryTester<'_> {
            fn assert(&mut self, query: impl Into<Query>, expected: Vec<(&'static str, &Author)>) {
                let query = query.into();
                let actual = self
                    .store
                    .get_many(self.namespace, query.clone())
                    .unwrap()
                    .map(|e| e.map(|e| (String::from_utf8(e.key().to_vec()).unwrap(), e.author())))
                    .collect::<Result<Vec<_>>>()
                    .unwrap();
                let expected = expected
                    .into_iter()
                    .map(|(key, author)| (key.to_string(), author.id()))
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "query: {query:#?}")
            }
        }

        let mut qt = QueryTester {
            store: &mut store,
            namespace: namespace_id,
        };

        qt.assert(
            Query::all(),
            vec![
                ("hi/world", &a1),
                ("hi/moon", &a2),
                ("hi/world", &a2),
                ("hi", &a3),
            ],
        );

        qt.assert(
            Query::single_latest_per_key(),
            vec![("hi", &a3), ("hi/moon", &a2), ("hi/world", &a1)],
        );

        qt.assert(
            Query::single_latest_per_key().key_prefix("hi/"),
            vec![("hi/moon", &a2), ("hi/world", &a1)],
        );

        qt.assert(
            Query::all().key_prefix("hi/"),
            vec![("hi/world", &a1), ("hi/moon", &a2), ("hi/world", &a2)],
        );

        qt.assert(
            Query::all().key_prefix("hi/").limit(1),
            vec![("hi/world", &a1)],
        );

        let mut replica = store.new_replica(namespace)?;
        replica.delete("hi/world", &a2).await?;
        let mut qt = QueryTester {
            store: &mut store,
            namespace: namespace_id,
        };

        qt.assert(
            Query::all(),
            vec![("hi/world", &a1), ("hi/moon", &a2), ("hi", &a3)],
        );

        qt.assert(
            Query::all().include_empty(),
            vec![
                ("hi/world", &a1),
                ("hi/moon", &a2),
                ("hi/world", &a2),
                ("hi", &a3),
            ],
        );
        store.flush()?;
        Ok(())
    }

    fn assert_keys(store: &mut Store, namespace: NamespaceId, mut expected: Vec<Vec<u8>>) {
        expected.sort();
        assert_eq!(expected, get_keys_sorted(store, namespace));
    }

    fn get_keys_sorted(store: &mut Store, namespace: NamespaceId) -> Vec<Vec<u8>> {
        let mut res = store
            .get_many(namespace, Query::all())
            .unwrap()
            .map(|e| e.map(|e| e.key().to_vec()))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        res.sort();
        res
    }

    fn get_entry(
        store: &mut Store,
        namespace: NamespaceId,
        author: AuthorId,
        key: &[u8],
    ) -> anyhow::Result<SignedEntry> {
        let entry = store
            .get_exact(namespace, author, key, true)?
            .ok_or_else(|| anyhow::anyhow!("not found"))?;
        Ok(entry)
    }

    fn get_content_hash(
        store: &mut Store,
        namespace: NamespaceId,
        author: AuthorId,
        key: &[u8],
    ) -> anyhow::Result<Option<Hash>> {
        let hash = store
            .get_exact(namespace, author, key, false)?
            .map(|e| e.content_hash());
        Ok(hash)
    }

    async fn sync<'a>(
        alice: &'a mut Replica<'a>,
        bob: &'a mut Replica<'a>,
    ) -> Result<(SyncOutcome, SyncOutcome)> {
        sync_filtered(alice, bob, None).await
    }

    /// Reconcile with `bob_filter` as bob's session egress — alice serves
    /// unfiltered, so the two sets read as divergent wherever the filter
    /// hides an entry bob holds.
    async fn sync_filtered<'a>(
        alice: &'a mut Replica<'a>,
        bob: &'a mut Replica<'a>,
        bob_filter: Option<crate::filter::EntryFilter>,
    ) -> Result<(SyncOutcome, SyncOutcome)> {
        let first = alice.sync_initial_message(None)?;
        sync_from_initial(alice, bob, first, bob_filter).await
    }

    /// Reconcile from an initial message the caller already took, so a test
    /// can write between session setup and the rounds — the order that
    /// makes a mid-session write mid-session.
    async fn sync_from_initial<'a>(
        alice: &'a mut Replica<'a>,
        bob: &'a mut Replica<'a>,
        first: crate::ranger::Message<SignedEntry>,
        bob_filter: Option<crate::filter::EntryFilter>,
    ) -> Result<(SyncOutcome, SyncOutcome)> {
        let (alice_state, bob_state, _) =
            sync_transcript(alice, bob, first, bob_filter, None).await?;
        Ok((alice_state, bob_state))
    }

    /// Reconcile with `bob_filter` and `bob_ingest` as bob's session access,
    /// returning every message of the session in the order it was sent.
    async fn sync_transcript<'a>(
        alice: &'a mut Replica<'a>,
        bob: &'a mut Replica<'a>,
        first: crate::ranger::Message<SignedEntry>,
        bob_filter: Option<crate::filter::EntryFilter>,
        bob_ingest: Option<crate::filter::SessionIngest>,
    ) -> Result<(
        SyncOutcome,
        SyncOutcome,
        Vec<crate::ranger::Message<SignedEntry>>,
    )> {
        let alice_peer_id = [1u8; 32];
        let bob_peer_id = [2u8; 32];
        let mut alice_state = SyncOutcome::default();
        let mut bob_state = SyncOutcome::default();
        let mut transcript = vec![first.clone()];
        let mut next_to_bob = Some(first);
        let mut rounds = 0;
        while let Some(msg) = next_to_bob.take() {
            assert!(rounds < 100, "too many rounds");
            rounds += 1;
            println!("round {rounds}");
            if let Some(msg) = bob
                .sync_process_message(
                    msg,
                    alice_peer_id,
                    &mut bob_state,
                    bob_filter.clone(),
                    bob_ingest.clone(),
                )
                .await?
            {
                transcript.push(msg.clone());
                next_to_bob = alice
                    .sync_process_message(msg, bob_peer_id, &mut alice_state, None, None)
                    .await?;
                transcript.extend(next_to_bob.clone());
            }
        }
        assert_eq!(alice_state.num_sent, bob_state.num_recv);
        assert_eq!(alice_state.num_recv, bob_state.num_sent);
        Ok((alice_state, bob_state, transcript))
    }

    /// Each key of `set` holds `author`'s entry whose content is the key itself.
    fn check_entries(
        store: &mut Store,
        namespace: &NamespaceId,
        author: &Author,
        set: &[&str],
    ) -> Result<()> {
        for el in set {
            let entry = store
                .get_exact(*namespace, author.id(), el, false)?
                .with_context(|| format!("{el} missing"))?;
            anyhow::ensure!(
                entry.content_hash() == Hash::new(el),
                "{el} holds other content"
            );
        }
        Ok(())
    }

    /// A record identifier decoded from outside carries its fixed head.
    ///
    /// Every accessor slices the namespace and the author unchecked, so a
    /// shorter value has to fail in the decoder: reaching a reader with one
    /// panics the thread that owns the store, for every namespace of every
    /// identity the node hosts.
    #[test]
    fn a_short_record_identifier_is_refused_by_the_decoder() {
        // postcard writes the inner byte string as a varint length followed
        // by that many bytes; every length below is one varint byte.
        for len in [0usize, 1, 31, 32, 63] {
            let mut encoded = vec![u8::try_from(len).unwrap()];
            encoded.extend(std::iter::repeat_n(0u8, len));
            assert!(
                postcard::from_bytes::<RecordIdentifier>(&encoded).is_err(),
                "a {len}-byte identifier decoded"
            );
        }

        let id = RecordIdentifier::new(NamespaceId::default(), AuthorId::default(), b"");
        let encoded = postcard::to_stdvec(&id).unwrap();
        assert_eq!(encoded.len(), 1 + ID_HEAD_BYTES);
        assert_eq!(
            postcard::from_bytes::<RecordIdentifier>(&encoded).unwrap(),
            id
        );
    }

    /// Snapshot of the `SignedEntry` postcard wire format used by doc sync.
    #[test]
    fn test_signed_entry_postcard_snapshot() {
        let author = Author::from_bytes(&[0xa1; 32]);
        let namespace = NamespaceSecret::from_bytes(&[0xb2; 32]);
        let record = Record::new(Hash::EMPTY, 0, 1_700_000_000_000_000u64);
        let id = RecordIdentifier::new(namespace.id(), author.id(), b"wire-format-test");
        let signed = SignedEntry::from_entry(Entry::new(id, record), &namespace, &author);

        let bytes = postcard::to_stdvec(&signed).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "4b523f1b6d9b00a4779fc9f8f105a9e36f062ceb7d511b632905782042ad30acb6dd07bfced4ecd5f3aa58321e8ace63f48f988ed8461bfdcd8b0e902187a10e228ddc6998329b7faa64875fe80da36406ea8d87e3e57bb048323e9cb66c0b343b60c4e709fb978b878e37d0c362edfc06c8cdc774c8b29d94e48eaa06cca60f5055154f42065ea5a1bea05463826be2684eb92df92c100027aabaae57ca554207bc7cbcb5636375fa1d82434d466724d92377f53b980695dd49d26d0ce12205a5776972652d666f726d61742d7465737400af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f32628080f9c0c1c48203",
        );

        let decoded: SignedEntry = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, signed);
    }

    /// Snapshot of the `Author` postcard format.
    #[test]
    fn test_author_postcard_snapshot() {
        let author = Author::from_bytes(&[0xa1; 32]);
        let bytes = postcard::to_stdvec(&author).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "20a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
        );
    }

    /// Snapshot of the `NamespaceSecret` postcard format.
    #[test]
    fn test_namespace_secret_postcard_snapshot() {
        let namespace = NamespaceSecret::from_bytes(&[0xb2; 32]);
        let bytes = postcard::to_stdvec(&namespace).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "20b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
        );
    }

    /// An older entry offered after a delete at its key is refused, so the
    /// delete stands.
    #[tokio::test]
    async fn an_older_entry_does_not_replace_a_delete_at_its_key() -> Result<()> {
        let mut store = store::Store::memory();
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let old = Entry::new(
            RecordIdentifier::new(namespace.id(), author.id(), b"k"),
            Record::new(Hash::new(b"v"), 1, 1_000),
        )
        .sign(&namespace, &author);
        let mut replica = store.new_replica(namespace.clone())?;
        replica
            .insert_remote_entry(old.clone(), [0u8; 32], ContentStatus::Complete)
            .await?;
        replica.delete(b"k", &author).await?;

        let res = replica
            .insert_remote_entry(old, [0u8; 32], ContentStatus::Complete)
            .await;
        assert!(matches!(res, Err(InsertError::NewerEntryExists)));
        let held = store
            .get_exact(namespace.id(), author.id(), b"k", true)?
            .expect("the delete is held");
        assert!(held.is_empty(), "the older entry replaced the delete");
        Ok(())
    }

    /// Two authors' entries at one key with equal timestamps read as the one
    /// with the greater content hash, whichever author holds it.
    ///
    /// The lower author gets the smaller hash: the author order alone picks
    /// the other entry.
    #[tokio::test]
    async fn an_equal_timestamp_across_authors_resolves_by_content_hash() -> Result<()> {
        let mut rng = rand::rng();
        let namespace = NamespaceSecret::new(&mut rng);
        let mut authors = [Author::new(&mut rng), Author::new(&mut rng)];
        authors.sort_by_key(|author| author.id().to_bytes());
        let [low, high] = &authors;
        let mut hashes = [Hash::new(b"a"), Hash::new(b"b")];
        hashes.sort();
        let [small, big] = hashes;
        let mut store = store::Store::memory();
        let mut replica = store.new_replica(namespace.clone())?;
        for (author, hash) in [(low, small), (high, big)] {
            let entry = Entry::new(
                RecordIdentifier::new(namespace.id(), author.id(), b"k"),
                Record::new(hash, 1, 1_000),
            )
            .sign(&namespace, author);
            replica
                .insert_remote_entry(entry, [0u8; 32], ContentStatus::Complete)
                .await?;
        }

        let read: Vec<_> = store
            .get_many(namespace.id(), Query::single_latest_per_key())?
            .collect::<Result<_>>()?;
        assert_eq!(read.len(), 1);
        assert_eq!(
            read.first().map(SignedEntry::content_hash),
            Some(big),
            "the tie went by author, not by content hash"
        );
        Ok(())
    }

    /// Two replicas converge on a delete whichever side opens the session,
    /// and stay converged over later sessions.
    #[tokio::test]
    async fn replicas_converge_on_a_delete_whichever_side_initiates() -> Result<()> {
        let mut rng = rand::rng();
        let author = Author::new(&mut rng);
        let namespace = NamespaceSecret::new(&mut rng);
        let old = Entry::new(
            RecordIdentifier::new(namespace.id(), author.id(), b"k"),
            Record::new(Hash::new(b"v"), 1, 1_000),
        )
        .sign(&namespace, &author);
        let mut writer_store = store::Store::memory();
        let mut holder_store = store::Store::memory();
        for store in [&mut writer_store, &mut holder_store] {
            store
                .new_replica(namespace.clone())?
                .insert_remote_entry(old.clone(), [0u8; 32], ContentStatus::Complete)
                .await?;
        }
        writer_store
            .new_replica(namespace.clone())?
            .delete(b"k", &author)
            .await?;

        // The writer opens first: the holder's older entry then reaches the
        // writer before the delete reaches the holder.
        for writer_initiates in [true, false, true, false] {
            {
                let mut writer = writer_store.new_replica(namespace.clone())?;
                let mut holder = holder_store.new_replica(namespace.clone())?;
                if writer_initiates {
                    sync(&mut writer, &mut holder).await?;
                } else {
                    sync(&mut holder, &mut writer).await?;
                }
            }
            for store in [&mut writer_store, &mut holder_store] {
                let held = store.get_exact(namespace.id(), author.id(), b"k", true)?;
                assert!(
                    held.is_some_and(|entry| entry.is_empty()),
                    "a replica lost the delete (writer initiated: {writer_initiates})"
                );
            }
        }
        Ok(())
    }
}
