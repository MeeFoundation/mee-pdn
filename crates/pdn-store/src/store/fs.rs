//! On disk storage for replicas.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    iter::{Chain, Flatten},
    num::NonZeroU64,
    ops::Bound,
};

use anyhow::{anyhow, Result};
use iroh::{KeyParsingError, PublicKey};
use iroh_blobs::Hash;
use n0_future::time::SystemTime;
use redb::{Database, ReadableDatabase, ReadableMultimapTable, ReadableTable};
use tracing::warn;

use super::{pubkeys::MemPublicKeyStore, ImportNamespaceOutcome, OpenError, PublicKeyStore, Query};
use crate::{
    actor::MAX_COMMIT_DELAY,
    keys::Author,
    ranger::{Fingerprint, Range, RangeEntry},
    sync::{Entry, EntrySignature, Record, RecordIdentifier, SignedEntry},
    AuthorHeads, AuthorId, Capability, CapabilityKind, NamespaceId, PeerIdBytes, ReplicaInfo,
};

mod bounds;
mod query;
mod ranges;
pub(crate) mod tables;

pub(crate) use self::ranges::RecordsRange;
use self::{
    bounds::{ByKeyBounds, RecordsBounds},
    query::QueryIterator,
    ranges::RangeExt,
    tables::{
        LatestPerAuthorKey, LatestPerAuthorValue, ReadOnlyTables, RecordsId, RecordsTable,
        RecordsValue, Tables, TransactionAndTables,
    },
};

/// Manages the replicas and authors for an instance.
#[derive(Debug)]
pub(crate) struct Store {
    db: Database,
    transaction: CurrentTransaction,
    open_replicas: HashSet<NamespaceId>,
    pubkeys: MemPublicKeyStore,
    /// How many times each namespace was written, counted where the write
    /// is applied. A caller comparing two replicas of one namespace asks
    /// this rather than a digest of their contents: it is the one value
    /// that moves on every mutation and on nothing else. In memory, so a
    /// restart resets it to zero and whoever compares treats the first
    /// look after a start as a change.
    writes: HashMap<NamespaceId, u64>,
    #[cfg(test)]
    commits: usize,
    /// Fails the next commit of a dirty transaction, as a full disk does.
    #[cfg(test)]
    fail_next_commit: bool,
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Err(err) = self.flush() {
            warn!("failed to trigger final flush: {:?}", err);
        }
    }
}

impl AsRef<Store> for Store {
    fn as_ref(&self) -> &Store {
        self
    }
}

impl AsMut<Store> for Store {
    fn as_mut(&mut self) -> &mut Store {
        self
    }
}

#[derive(derive_more::Debug, Default)]
#[allow(clippy::large_enum_variant)]
enum CurrentTransaction {
    #[default]
    None,
    Read(ReadOnlyTables),
    Write(TransactionAndTables),
}

fn open_database(path: &std::path::Path, cache_bytes: usize) -> Result<Database> {
    match Database::builder().set_cache_size(cache_bytes).create(path) {
        Ok(db) => Ok(db),
        Err(redb::DatabaseError::UpgradeRequired(v)) => Err(anyhow!(
            "Opening the database failed: Upgrading from redb {v} no longer supported. Use an older redb version first."
        )),
        Err(err) => Err(err.into()),
    }
}

impl Store {
    /// Create a new store in memory.
    pub fn memory() -> Self {
        Self::memory_impl().expect("failed to create memory store")
    }

    fn memory_impl() -> Result<Self> {
        let db = Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?;
        Self::new_impl(db)
    }

    /// Create or open a store from a `path` to a database file.
    ///
    /// The file will be created if it does not exist, otherwise it will be opened.
    ///
    /// `cache_bytes` caps the resident pages this store holds; it does not
    /// reserve them. The bound cannot be changed on an open store, so a
    /// consumer that divides one budget among several stores cuts the
    /// share before opening any of them.
    pub fn persistent(path: impl AsRef<std::path::Path>, cache_bytes: usize) -> Result<Self> {
        Self::new_impl(open_database(path.as_ref(), cache_bytes)?)
    }

    fn new_impl(db: redb::Database) -> Result<Self> {
        // Setup all tables
        let write_tx = db.begin_write()?;
        let _ = Tables::new(&write_tx)?;
        write_tx.commit()?;

        Ok(Store {
            db,
            transaction: Default::default(),
            open_replicas: Default::default(),
            pubkeys: Default::default(),
            writes: Default::default(),
            #[cfg(test)]
            commits: 0,
            #[cfg(test)]
            fail_next_commit: false,
        })
    }

    /// Flush the current transaction, if any.
    ///
    /// This is the cheapest way to ensure that the data is persisted.
    pub fn flush(&mut self) -> Result<()> {
        if let CurrentTransaction::Write(w) = std::mem::take(&mut self.transaction) {
            self.finish_write(w)?;
        }
        Ok(())
    }

    /// Commit a write transaction, or drop it when nothing was written
    /// through it.
    ///
    /// redb commits durably and has no path that skips the file sync for a
    /// transaction that changed nothing, so committing a clean one buys a
    /// sync for nothing. Clean is the ordinary case rather than a rarity:
    /// reads open a write transaction too ([`Store::tables`]), and every
    /// sync session opens a snapshot, which flushes. Dropping the
    /// transaction instead abandons no data — there is none — and leaves
    /// the same committed state for a reader to see.
    ///
    /// The skip has a second effect: a commit is also where redb returns
    /// pages freed under a since-closed read transaction to the allocator,
    /// so on a node that holds no writes those pages wait for the next real
    /// one.
    fn finish_write(&mut self, w: TransactionAndTables) -> Result<()> {
        if !w.is_dirty() {
            return Ok(());
        }
        #[cfg(test)]
        {
            if std::mem::take(&mut self.fail_next_commit) {
                return Err(anyhow!("the commit failed for test"));
            }
            self.commits += 1;
        }
        w.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_next_commit(&mut self) {
        self.fail_next_commit = true;
    }

    /// How many write transactions were actually committed (test
    /// observability for the clean-transaction skip).
    #[cfg(test)]
    pub(crate) fn debug_commit_count(&self) -> usize {
        self.commits
    }

    /// Which transaction is currently held, so a test asserting about
    /// clean commits can show it had one to skip.
    #[cfg(test)]
    pub(crate) fn debug_transaction_kind(&self) -> &'static str {
        match self.transaction {
            CurrentTransaction::None => "none",
            CurrentTransaction::Read(_) => "read",
            CurrentTransaction::Write(_) => "write",
        }
    }

    /// Get a read-only snapshot of the database.
    ///
    /// This has the side effect of committing any open write transaction,
    /// so it can be used as a way to ensure that the data is persisted.
    pub fn snapshot(&mut self) -> Result<&ReadOnlyTables> {
        let tables = match std::mem::take(&mut self.transaction) {
            CurrentTransaction::None => {
                let tx = self.db.begin_read()?;
                ReadOnlyTables::new(tx)?
            }
            CurrentTransaction::Write(w) => {
                self.finish_write(w)?;
                let tx = self.db.begin_read()?;
                ReadOnlyTables::new(tx)?
            }
            CurrentTransaction::Read(tables) => tables,
        };
        self.transaction = CurrentTransaction::Read(tables);
        match &self.transaction {
            CurrentTransaction::Read(ref tables) => Ok(tables),
            _ => unreachable!(),
        }
    }

    /// Get an owned read-only snapshot of the database.
    ///
    /// This will open a new read transaction. The read transaction won't be reused for other
    /// reads.
    ///
    /// This has the side effect of committing any open write transaction,
    /// so it can be used as a way to ensure that the data is persisted.
    pub(crate) fn snapshot_owned(&mut self) -> Result<ReadOnlyTables> {
        // make sure the current transaction is committed
        self.flush()?;
        assert!(matches!(self.transaction, CurrentTransaction::None));
        let tx = self.db.begin_read()?;
        let tables = ReadOnlyTables::new(tx)?;
        Ok(tables)
    }

    /// Get access to the tables to read from them.
    ///
    /// The underlying transaction is a write transaction, but with a non-mut
    /// reference to the tables you can not write.
    ///
    /// There is no guarantee that this will be an independent transaction.
    /// You just get readonly access to the current state of the database.
    ///
    /// As such, there is also no guarantee that the data you see is
    /// already persisted.
    fn tables(&mut self) -> Result<&Tables<'_>> {
        let tables = self.current_write_transaction()?;
        self.transaction = CurrentTransaction::Write(tables);
        match &mut self.transaction {
            CurrentTransaction::Write(ref mut tables) => Ok(tables.tables()),
            _ => unreachable!(),
        }
    }

    /// The write transaction to work in: the open one while it is young
    /// enough, a fresh one otherwise.
    fn current_write_transaction(&mut self) -> Result<TransactionAndTables> {
        match std::mem::take(&mut self.transaction) {
            CurrentTransaction::Write(w) if w.since.elapsed() <= MAX_COMMIT_DELAY => Ok(w),
            CurrentTransaction::Write(w) => {
                tracing::debug!("committing transaction because it's too old");
                self.finish_write(w)?;
                let tx = self.db.begin_write()?;
                Ok(TransactionAndTables::new(tx)?)
            }
            CurrentTransaction::None | CurrentTransaction::Read(_) => {
                let tx = self.db.begin_write()?;
                Ok(TransactionAndTables::new(tx)?)
            }
        }
    }

    /// Get exclusive write access to the tables in the current transaction.
    ///
    /// There is no guarantee that this will be an independent transaction.
    /// As such, there is also no guarantee that the data you see or write
    /// will be persisted.
    ///
    /// To ensure that the data is persisted, acquire a snapshot of the database
    /// or call flush.
    fn modify<T>(&mut self, f: impl FnOnce(&mut Tables) -> Result<T>) -> Result<T> {
        let tables = self.current_write_transaction()?;
        self.transaction = CurrentTransaction::Write(tables);
        let res = match &mut self.transaction {
            CurrentTransaction::Write(ref mut tables) => tables.with_tables_mut(f)?,
            _ => unreachable!(),
        };
        Ok(res)
    }
}

type PeersIter = std::vec::IntoIter<PeerIdBytes>;

impl Store {
    /// Create a new replica for `namespace` and persist in this store.
    #[cfg(test)]
    pub(crate) fn new_replica(
        &mut self,
        namespace: crate::NamespaceSecret,
    ) -> Result<crate::sync::Replica<'_>> {
        let id = namespace.id();
        self.import_namespace(namespace.into())?;
        self.open_replica(&id).map_err(Into::into)
    }

    /// Create a new author key and persist it in the store.
    #[cfg(test)]
    pub(crate) fn new_author<R: rand::CryptoRng>(&mut self, rng: &mut R) -> Result<Author> {
        let author = Author::new(rng);
        self.import_author(author.clone())?;
        Ok(author)
    }

    /// Check if a [`AuthorHeads`] contains entry timestamps that we do not have locally.
    ///
    /// Returns the number of authors that the other peer has updates for.
    pub(crate) fn has_news_for_us(
        &mut self,
        namespace: NamespaceId,
        heads: &AuthorHeads,
    ) -> Result<Option<NonZeroU64>> {
        let our_heads = {
            let latest = self.get_latest_for_each_author(namespace)?;
            let mut heads = AuthorHeads::default();
            for e in latest {
                let (author, timestamp, _key) = e?;
                heads.insert(author, timestamp);
            }
            heads
        };
        let has_news_for_us = heads.has_news_for(&our_heads);
        Ok(has_news_for_us)
    }

    /// Open a replica from this store.
    ///
    /// This just calls load_replica_info and then creates a new replica with the info.
    #[cfg(test)]
    pub(crate) fn open_replica(
        &mut self,
        namespace_id: &NamespaceId,
    ) -> Result<crate::sync::Replica<'_>, OpenError> {
        let info = self.load_replica_info(namespace_id)?;
        let instance = StoreInstance::new(*namespace_id, self);
        Ok(crate::sync::Replica::new(instance, Box::new(info)))
    }

    /// Load the replica info from the store.
    pub(crate) fn load_replica_info(
        &mut self,
        namespace_id: &NamespaceId,
    ) -> Result<ReplicaInfo, OpenError> {
        let tables = self.tables()?;
        let info = match tables.namespaces.get(namespace_id.as_bytes()) {
            Ok(Some(db_value)) => {
                let (raw_kind, raw_bytes) = db_value.value();
                let namespace = Capability::from_raw(raw_kind, raw_bytes)?;
                ReplicaInfo::new(namespace)
            }
            Ok(None) => return Err(OpenError::NotFound),
            Err(err) => return Err(OpenError::Other(err.into())),
        };
        self.open_replicas.insert(info.capability.id());
        Ok(info)
    }

    /// Close a replica.
    pub(crate) fn close_replica(&mut self, id: NamespaceId) {
        self.open_replicas.remove(&id);
    }

    /// List all replica namespaces in this store.
    pub(crate) fn list_namespaces(
        &mut self,
    ) -> Result<impl Iterator<Item = Result<(NamespaceId, CapabilityKind)>>> {
        let snapshot = self.snapshot()?;
        let iter = snapshot.namespaces.range::<&'static [u8; 32]>(..)?;
        let iter = iter.map(|res| {
            let capability = parse_capability(res?.1.value())?;
            Ok((capability.id(), capability.kind()))
        });
        Ok(iter)
    }

    /// Get an author key from the store.
    pub(crate) fn get_author(&mut self, author_id: &AuthorId) -> Result<Option<Author>> {
        let tables = self.tables()?;
        let Some(author) = tables.authors.get(author_id.as_bytes())? else {
            return Ok(None);
        };
        let author = Author::from_bytes(author.value());
        Ok(Some(author))
    }

    /// Import an author key pair.
    pub(crate) fn import_author(&mut self, author: Author) -> Result<()> {
        self.modify(|tables| {
            tables
                .authors
                .insert(author.id().as_bytes(), &author.to_bytes())?;
            Ok(())
        })
    }

    /// Import a new replica namespace.
    pub fn import_namespace(&mut self, capability: Capability) -> Result<ImportNamespaceOutcome> {
        self.modify(|tables| {
            let id = capability.id().to_bytes();
            if tables.namespaces.get(&id)?.is_some() {
                return Ok(ImportNamespaceOutcome::NoChange);
            }
            let (kind, bytes) = capability.raw();
            tables.namespaces.insert(&id, (kind, &bytes))?;
            Ok(ImportNamespaceOutcome::Inserted)
        })
    }

    /// Count one write of `namespace`. Called where the write is applied,
    /// so nothing — a subscription, a channel, a healthy actor — stands
    /// between a mutation and the count of it.
    pub(crate) fn wrote(&mut self, namespace: NamespaceId) {
        *self.writes.entry(namespace).or_default() += 1;
    }

    /// How many writes this replica has taken since the store opened. Two
    /// replicas of one namespace are told apart by comparing each against
    /// its own earlier reading, never against the other's: the number
    /// counts writes, not entries.
    pub(crate) fn writes_of(&self, namespace: NamespaceId) -> u64 {
        self.writes.get(&namespace).copied().unwrap_or_default()
    }

    /// Remove a replica.
    ///
    /// Completely removes a replica and deletes both the namespace private key and all document
    /// entries.
    ///
    /// Note that a replica has to be closed before it can be removed. The store has to enforce
    /// that a replica cannot be removed while it is still open.
    pub(crate) fn remove_replica(&mut self, namespace: &NamespaceId) -> Result<()> {
        if self.open_replicas.contains(namespace) {
            return Err(anyhow!("replica is not closed"));
        }
        self.modify(|tables| {
            let bounds = RecordsBounds::namespace(*namespace);
            tables.records.retain_in(bounds.as_ref(), |_k, _v| false)?;
            let bounds = ByKeyBounds::namespace(*namespace);
            let _ = tables
                .records_by_key
                .retain_in(bounds.as_ref(), |_k, _v| false);
            tables.namespaces.remove(namespace.as_bytes())?;
            tables.namespace_peers.remove_all(namespace.as_bytes())?;
            Ok(())
        })
    }

    /// Get an iterator over entries of a replica.
    pub fn get_many(
        &mut self,
        namespace: NamespaceId,
        query: impl Into<Query>,
    ) -> Result<QueryIterator> {
        let tables = self.snapshot_owned()?;
        QueryIterator::new(tables, namespace, query.into())
    }

    /// Get an entry by key and author.
    pub fn get_exact(
        &mut self,
        namespace: NamespaceId,
        author: AuthorId,
        key: impl AsRef<[u8]>,
        include_empty: bool,
    ) -> Result<Option<SignedEntry>> {
        get_exact(
            &self.tables()?.records,
            namespace,
            author,
            key,
            include_empty,
        )
    }

    /// Get all content hashes of all replicas in the store.
    pub(crate) fn content_hashes(&mut self) -> Result<ContentHashesIterator> {
        let tables = self.snapshot_owned()?;
        ContentHashesIterator::all(&tables.records)
    }

    /// Get the latest entry for each author in a namespace.
    pub(crate) fn get_latest_for_each_author(
        &mut self,
        namespace: NamespaceId,
    ) -> Result<LatestIterator<'_>> {
        LatestIterator::new(&self.tables()?.latest_per_author, namespace)
    }

    /// Register a peer that has been useful to sync a document.
    pub(crate) fn register_useful_peer(
        &mut self,
        namespace: NamespaceId,
        peer: crate::PeerIdBytes,
    ) -> Result<()> {
        let peer = &peer;
        let namespace = namespace.as_bytes();
        // calculate nanos since UNIX_EPOCH for a time measurement
        let nanos = SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|duration| duration.as_nanos() as u64)?;
        self.modify(|tables| {
            // ensure the document exists
            anyhow::ensure!(
                tables.namespaces.get(namespace)?.is_some(),
                "document not created"
            );

            let mut namespace_peers = tables.namespace_peers.get(namespace)?;

            // get the oldest entry since it's candidate for removal
            let maybe_oldest = namespace_peers.next().transpose()?.map(|guard| {
                let (oldest_nanos, &oldest_peer) = guard.value();
                (oldest_nanos, oldest_peer)
            });
            match maybe_oldest {
                None => {
                    // the table is empty so the peer can be inserted without further checks since
                    // super::PEERS_PER_DOC_CACHE_SIZE is non zero
                    drop(namespace_peers);
                    tables.namespace_peers.insert(namespace, (nanos, peer))?;
                }
                Some((oldest_nanos, oldest_peer)) => {
                    let oldest_peer = &oldest_peer;

                    if oldest_peer == peer {
                        // oldest peer is the current one, so replacing the entry for the peer will
                        // maintain the size
                        drop(namespace_peers);
                        tables
                            .namespace_peers
                            .remove(namespace, (oldest_nanos, oldest_peer))?;
                        tables.namespace_peers.insert(namespace, (nanos, peer))?;
                    } else {
                        // calculate the len in the same loop since calling `len` is another fallible operation
                        let mut len = 1;
                        // find any previous entry for the same peer to remove it
                        let mut prev_peer_nanos = None;

                        for result in namespace_peers {
                            len += 1;
                            let guard = result?;
                            let (peer_nanos, peer_bytes) = guard.value();
                            if prev_peer_nanos.is_none() && peer_bytes == peer {
                                prev_peer_nanos = Some(peer_nanos)
                            }
                        }

                        match prev_peer_nanos {
                            Some(prev_nanos) => {
                                // the peer was already present, so we can remove the old entry and
                                // insert the new one without checking the size
                                tables
                                    .namespace_peers
                                    .remove(namespace, (prev_nanos, peer))?;
                                tables.namespace_peers.insert(namespace, (nanos, peer))?;
                            }
                            None => {
                                // the peer is new and the table is non empty, add it and check the
                                // size to decide if the oldest peer should be evicted
                                tables.namespace_peers.insert(namespace, (nanos, peer))?;
                                len += 1;
                                if len > super::PEERS_PER_DOC_CACHE_SIZE.get() {
                                    tables
                                        .namespace_peers
                                        .remove(namespace, (oldest_nanos, oldest_peer))?;
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Get the peers that have been useful for a document.
    pub fn get_sync_peers(&mut self, namespace: &NamespaceId) -> Result<Option<PeersIter>> {
        let tables = self.tables()?;
        let mut peers = Vec::with_capacity(super::PEERS_PER_DOC_CACHE_SIZE.get());
        for result in tables.namespace_peers.get(namespace.as_bytes())?.rev() {
            let (_nanos, &peer) = result?.value();
            peers.push(peer);
        }
        if peers.is_empty() {
            Ok(None)
        } else {
            Ok(Some(peers.into_iter()))
        }
    }
}

impl PublicKeyStore for Store {
    fn public_key(&self, id: &[u8; 32]) -> Result<PublicKey, KeyParsingError> {
        self.pubkeys.public_key(id)
    }
}

fn parse_capability((raw_kind, raw_bytes): (u8, &[u8; 32])) -> Result<Capability> {
    Capability::from_raw(raw_kind, raw_bytes)
}

fn get_exact(
    record_table: &impl ReadableTable<RecordsId<'static>, RecordsValue<'static>>,
    namespace: NamespaceId,
    author: AuthorId,
    key: impl AsRef<[u8]>,
    include_empty: bool,
) -> Result<Option<SignedEntry>> {
    let id = (namespace.as_bytes(), author.as_bytes(), key.as_ref());
    let record = record_table.get(id)?;
    Ok(record
        .map(|r| into_entry(id, r.value()))
        .filter(|entry| include_empty || !entry.is_empty()))
}

/// A wrapper around [`Store`] for a specific [`NamespaceId`]
#[derive(Debug)]
pub(crate) struct StoreInstance<'a> {
    namespace: NamespaceId,
    pub(crate) store: &'a mut Store,
    /// Frozen read source for one sync session's egress.
    ///
    /// When set, the reads that serve the peer — `get_first`, `get_range`,
    /// and the fingerprints computed from them — come from this snapshot,
    /// so the served view is stable for the whole session while writes
    /// continue on the live store. Ingest stays live: `entry_get` and
    /// `entry_put` always operate on the current state, so the newer-than
    /// comparison of `ranger::Store::put` is not judged against a stale
    /// snapshot, where an older entry could overwrite a concurrent
    /// session's insert.
    pub(crate) session_snapshot: Option<&'a ReadOnlyTables>,
}

impl<'a> StoreInstance<'a> {
    pub(crate) fn new(namespace: NamespaceId, store: &'a mut Store) -> Self {
        StoreInstance {
            namespace,
            store,
            session_snapshot: None,
        }
    }

    /// Every namespace's records share one table, so an identifier naming
    /// another namespace would read — or, through `put`, overwrite — that
    /// namespace's rows. Refused, as `get_range` refuses a foreign boundary.
    fn ensure_own_namespace(&self, id: &RecordIdentifier) -> Result<()> {
        anyhow::ensure!(
            id.namespace() == self.namespace,
            "record identifier names another namespace"
        );
        Ok(())
    }

    /// The instance a sync session reads through.
    pub(crate) fn with_session_snapshot(
        namespace: NamespaceId,
        store: &'a mut Store,
        session_snapshot: Option<&'a ReadOnlyTables>,
    ) -> Self {
        StoreInstance {
            namespace,
            store,
            session_snapshot,
        }
    }
}

/// The first record in `bounds`, or `None` when the range is empty.
fn first_record(
    records: &impl ReadableTable<RecordsId<'static>, RecordsValue<'static>>,
    bounds: &RecordsBounds,
) -> Result<Option<RecordIdentifier>> {
    records
        .range(bounds.as_ref())?
        .next_map(|(namespace, author, key), _| RecordIdentifier::new(namespace, author, key))
        .transpose()
}

/// The range iterators for `bounds`, plus the wrap-around half when the
/// range has one. One helper for both sources, so the snapshot and the live
/// store cannot come to build their ranges differently.
fn range_pair<'a>(
    records: &'a impl ReadableTable<RecordsId<'static>, RecordsValue<'static>>,
    bounds: RecordsBounds,
    wrap_bounds: Option<RecordsBounds>,
) -> Result<(RecordsRange<'a>, Option<RecordsRange<'a>>)> {
    Ok((
        RecordsRange::with_bounds(records, bounds)?,
        wrap_bounds
            .map(|b| RecordsRange::with_bounds(records, b))
            .transpose()?,
    ))
}

impl PublicKeyStore for StoreInstance<'_> {
    fn public_key(&self, id: &[u8; 32]) -> std::result::Result<PublicKey, KeyParsingError> {
        self.store.public_key(id)
    }
}

impl<'a> crate::ranger::Store<SignedEntry> for StoreInstance<'a> {
    type Error = anyhow::Error;
    type RangeIterator<'x>
        = Chain<RecordsRange<'x>, Flatten<std::option::IntoIter<RecordsRange<'x>>>>
    where
        'a: 'x;

    /// Get a the first key (or the default if none is available).
    fn get_first(&mut self) -> Result<RecordIdentifier> {
        // TODO: verify this fetches all keys with this namespace
        let bounds = RecordsBounds::namespace(self.namespace);
        let first = match self.session_snapshot {
            Some(tables) => first_record(&tables.records, &bounds)?,
            None => first_record(&self.store.as_mut().tables()?.records, &bounds)?,
        };
        Ok(first.unwrap_or_default())
    }

    /// This and the other test-only readers below — `len`, `is_empty`,
    /// `all`, `prefixed_by` — read the live store whatever
    /// `session_snapshot` holds, so a test written through them
    /// characterises something other than the frozen view.
    #[cfg(test)]
    fn get(&mut self, id: &RecordIdentifier) -> Result<Option<SignedEntry>> {
        self.store
            .as_mut()
            .get_exact(id.namespace(), id.author(), id.key(), true)
    }

    #[cfg(test)]
    fn len(&mut self) -> Result<usize> {
        let tables = self.store.as_mut().tables()?;
        let bounds = RecordsBounds::namespace(self.namespace);
        let records = tables.records.range(bounds.as_ref())?;
        Ok(records.count())
    }

    #[cfg(test)]
    fn is_empty(&mut self) -> Result<bool> {
        use redb::ReadableTableMetadata;
        let tables = self.store.as_mut().tables()?;
        Ok(tables.records.is_empty()?)
    }

    fn get_fingerprint(&mut self, range: &Range<RecordIdentifier>) -> Result<Fingerprint> {
        // TODO: optimize
        let elements = self.get_range(range.clone())?;

        let mut fp = Fingerprint::empty();
        for el in elements {
            let el = el?;
            fp ^= el.as_fingerprint();
        }

        Ok(fp)
    }

    fn entry_put(&mut self, e: SignedEntry) -> Result<()> {
        let id = e.id();
        self.ensure_own_namespace(id)?;
        self.store.wrote(id.namespace());
        self.store.as_mut().modify(|tables| {
            // insert into record table
            let key = (
                &id.namespace().to_bytes(),
                &id.author().to_bytes(),
                id.key(),
            );
            let hash = e.content_hash(); // let binding is needed
            let value = (
                e.timestamp(),
                &e.signature().namespace().to_bytes(),
                &e.signature().author().to_bytes(),
                e.content_len(),
                hash.as_bytes(),
            );
            tables.records.insert(key, value)?;

            // insert into by key index table
            let key = (
                &id.namespace().to_bytes(),
                id.key(),
                &id.author().to_bytes(),
            );
            tables.records_by_key.insert(key, ())?;

            // The latest table keeps the author's greatest timestamp, not
            // the one written last: entries arrive out of order — a
            // reconciliation catching up on an older one is the ordinary
            // case — and a value that moved backwards would make this
            // replica report less than it holds, so a peer comparing
            // against it would stop offering what it has.
            let key = (&e.id().namespace().to_bytes(), &e.id().author().to_bytes());
            let latest = tables
                .latest_per_author
                .get(key)?
                .map(|value| value.value().0)
                .unwrap_or_default();
            if e.timestamp() >= latest {
                let value = (e.timestamp(), e.id().key());
                tables.latest_per_author.insert(key, value)?;
            }
            Ok(())
        })
    }

    fn get_range(&mut self, range: Range<RecordIdentifier>) -> Result<Self::RangeIterator<'_>> {
        let order = range.x().cmp(range.y());
        // Every namespace's records share one table and these boundaries
        // arrive from the peer, so an unchecked one scans another
        // namespace's rows and serves them. Reconciliation derives every
        // boundary from a key of the replica under exchange, so a foreign
        // one is a protocol violation rather than a request to narrow. The
        // identity range (x == y) carries no boundary — it means this
        // replica whole — and its `x` is the default identifier on an empty
        // replica, which names no namespace at all.
        if order != Ordering::Equal {
            for boundary in [range.x(), range.y()] {
                anyhow::ensure!(
                    boundary.namespace() == self.namespace,
                    "sync range boundary names another namespace"
                );
            }
        }
        let (bounds, wrap_bounds) = match order {
            // identity range: all entries in the replica, no wrap-around
            Ordering::Equal => (RecordsBounds::namespace(self.namespace), None),
            // regular range: x <= t < y, no wrap-around
            Ordering::Less => {
                let start = Bound::Included(range.x().to_byte_tuple());
                let end = Bound::Excluded(range.y().to_byte_tuple());
                (RecordsBounds::new(start, end), None)
            }
            // split range: start <= t < y, then wrap-around x <= t <= end
            Ordering::Greater => {
                let end = Bound::Excluded(range.y().to_byte_tuple());
                let bounds = RecordsBounds::from_start(&self.namespace, end);
                let start = Bound::Included(range.x().to_byte_tuple());
                let wrap_bounds = RecordsBounds::to_end(&self.namespace, start);
                (bounds, Some(wrap_bounds))
            }
        };
        let (iter, wrap_iter) = match self.session_snapshot {
            Some(tables) => range_pair(&tables.records, bounds, wrap_bounds)?,
            None => range_pair(&self.store.as_mut().tables()?.records, bounds, wrap_bounds)?,
        };
        Ok(iter.chain(wrap_iter.into_iter().flatten()))
    }

    #[cfg(test)]
    fn entry_remove(&mut self, id: &RecordIdentifier) -> Result<Option<SignedEntry>> {
        self.store.wrote(id.namespace());
        self.store.as_mut().modify(|tables| {
            let entry = {
                let (namespace, author, key) = id.as_byte_tuple();
                let id = (namespace, key, author);
                tables.records_by_key.remove(id)?;
                let id = (namespace, author, key);
                let value = tables.records.remove(id)?;
                value.map(|value| into_entry(id, value.value()))
            };
            Ok(entry)
        })
    }

    #[cfg(test)]
    fn all(&mut self) -> Result<Self::RangeIterator<'_>> {
        let tables = self.store.as_mut().tables()?;
        let bounds = RecordsBounds::namespace(self.namespace);
        let iter = RecordsRange::with_bounds(&tables.records, bounds)?;
        Ok(chain_none(iter))
    }

    fn entry_get(&mut self, id: &RecordIdentifier) -> Result<Option<SignedEntry>> {
        self.ensure_own_namespace(id)?;
        self.store
            .as_mut()
            .get_exact(id.namespace(), id.author(), id.key(), true)
    }

    #[cfg(test)]
    fn prefixed_by(&mut self, id: &RecordIdentifier) -> Result<Self::RangeIterator<'_>> {
        self.ensure_own_namespace(id)?;
        let tables = self.store.as_mut().tables()?;
        let bounds = RecordsBounds::author_prefix(self.namespace, id.author(), id.key_bytes());
        let iter = RecordsRange::with_bounds(&tables.records, bounds)?;
        Ok(chain_none(iter))
    }
}

#[cfg(test)]
fn chain_none<'a, I: Iterator<Item = T> + 'a, T>(
    iter: I,
) -> Chain<I, Flatten<std::option::IntoIter<I>>> {
    iter.chain(None.into_iter().flatten())
}

/// Iterator for all content hashes
///
/// Note that you might get duplicate hashes. Also, the iterator will keep
/// a database snapshot open until it is dropped.
///
/// Also, this represents a snapshot of the database at the time of creation.
/// It needs a copy of a redb::ReadOnlyTable to be self-contained.
#[derive(derive_more::Debug)]
pub(crate) struct ContentHashesIterator {
    #[debug(skip)]
    range: RecordsRange<'static>,
}

impl ContentHashesIterator {
    /// Create a new iterator over all content hashes.
    pub fn all(table: &RecordsTable) -> anyhow::Result<Self> {
        let range = RecordsRange::all_static(table)?;
        Ok(Self { range })
    }
}

impl Iterator for ContentHashesIterator {
    type Item = Result<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        let v = self.range.next()?;
        Some(v.map(|e| e.content_hash()))
    }
}

/// Iterator over the latest entry per author.
#[derive(derive_more::Debug)]
#[debug("LatestIterator")]
pub(crate) struct LatestIterator<'a>(
    redb::Range<'a, LatestPerAuthorKey<'static>, LatestPerAuthorValue<'static>>,
);

impl<'a> LatestIterator<'a> {
    fn new(
        latest_per_author: &'a impl ReadableTable<
            LatestPerAuthorKey<'static>,
            LatestPerAuthorValue<'static>,
        >,
        namespace: NamespaceId,
    ) -> anyhow::Result<Self> {
        let start = (namespace.as_bytes(), &[u8::MIN; 32]);
        let end = (namespace.as_bytes(), &[u8::MAX; 32]);
        let range = latest_per_author.range(start..=end)?;
        Ok(Self(range))
    }
}

impl Iterator for LatestIterator<'_> {
    type Item = Result<(AuthorId, u64, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_map(|key, value| {
            let (_namespace, author) = key;
            let (timestamp, key) = value;
            (author.into(), timestamp, key.to_vec())
        })
    }
}

fn into_entry(key: RecordsId, value: RecordsValue) -> SignedEntry {
    let (namespace, author, key) = key;
    let (timestamp, namespace_sig, author_sig, len, hash) = value;
    let id = RecordIdentifier::new(namespace, author, key);
    let record = Record::unchecked(hash.into(), len, timestamp);
    let entry = Entry::new(id, record);
    let entry_signature = EntrySignature::from_parts(namespace_sig, author_sig);
    SignedEntry::new(entry_signature, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ranger::Store as _, NamespaceSecret};

    /// A cache big enough that nothing in a scenario evicts.
    const TEST_CACHE_BYTES: usize = 16 * 1024 * 1024;

    #[tokio::test]
    async fn test_ranges() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let mut store = Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;

        let author = store.new_author(&mut rand::rng())?;
        let namespace = NamespaceSecret::new(&mut rand::rng());
        let mut replica = store.new_replica(namespace.clone())?;

        // test author prefix relation for all-255 keys
        let key1 = vec![255, 255];
        let key2 = vec![255, 255, 255];
        replica.hash_and_insert(&key1, &author, b"v1").await?;
        replica.hash_and_insert(&key2, &author, b"v2").await?;
        let res = store
            .get_many(namespace.id(), Query::author(author.id()).key_prefix([255]))?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(res.len(), 2);
        assert_eq!(
            res.into_iter()
                .map(|entry| entry.key().to_vec())
                .collect::<Vec<_>>(),
            vec![key1, key2]
        );
        Ok(())
    }

    /// A persistent store's cache holds to the bound it was opened with: a
    /// working set many times the bound evicts, and what stays resident fits
    /// it. Were the bound lost on the way to the database, the store would
    /// open at redb's own default, which nothing here comes near.
    #[tokio::test]
    async fn a_persistent_store_opens_its_cache_at_the_bound_it_is_given() -> Result<()> {
        const BOUND: usize = 256 * 1024;
        let dbfile = tempfile::NamedTempFile::new()?;
        let mut store = Store::persistent(dbfile.path(), BOUND)?;
        let author = store.new_author(&mut rand::rng())?;
        let namespace = NamespaceSecret::new(&mut rand::rng());
        {
            let mut replica = store.new_replica(namespace.clone())?;
            for i in 0..8_000u32 {
                replica
                    .hash_and_insert(format!("key/{i}"), &author, i.to_be_bytes())
                    .await?;
            }
        }
        store.flush()?;
        let read = store.get_many(namespace.id(), Query::all())?.count();
        assert_eq!(read, 8_000);

        let stats = store.db.cache_stats();
        assert!(
            stats.evictions() > 0,
            "a working set past the bound evicted nothing: the bound did not reach the database"
        );
        assert!(
            stats.used_bytes() <= BOUND,
            "the cache holds {} bytes past its bound of {BOUND}",
            stats.used_bytes()
        );
        Ok(())
    }

    #[test]
    fn test_basics() -> Result<()> {
        let dbfile = tempfile::NamedTempFile::new()?;
        let mut store = Store::persistent(dbfile.path(), TEST_CACHE_BYTES)?;

        let author = store.new_author(&mut rand::rng())?;
        let namespace = NamespaceSecret::new(&mut rand::rng());
        let _replica = store.new_replica(namespace.clone())?;
        store.close_replica(namespace.id());
        let replica = store.load_replica_info(&namespace.id())?;
        assert_eq!(replica.capability.id(), namespace.id());

        let author_back = store.get_author(&author.id())?.unwrap();
        assert_eq!(author.to_bytes(), author_back.to_bytes(),);

        let mut wrapper = StoreInstance::new(namespace.id(), &mut store);
        for i in 0..5 {
            let id = RecordIdentifier::new(namespace.id(), author.id(), format!("hello-{i}"));
            let entry = Entry::new(id, Record::current_from_data(format!("world-{i}")));
            let entry = SignedEntry::from_entry(entry, &namespace, &author);
            wrapper.entry_put(entry)?;
        }

        // all
        let all: Vec<_> = wrapper.all()?.collect();
        assert_eq!(all.len(), 5);

        // add a second version
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = RecordIdentifier::new(namespace.id(), author.id(), format!("hello-{i}"));
            let entry = Entry::new(
                id.clone(),
                Record::current_from_data(format!("world-{i}-2")),
            );
            let entry = SignedEntry::from_entry(entry, &namespace, &author);
            wrapper.entry_put(entry)?;
            ids.push(id);
        }

        // get all
        let entries = wrapper
            .store
            .get_many(namespace.id(), Query::all())?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(entries.len(), 5);

        // get all prefix
        let entries = wrapper
            .store
            .get_many(namespace.id(), Query::key_prefix("hello-"))?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(entries.len(), 5);

        // delete and get
        for id in ids {
            let res = wrapper.get(&id)?;
            assert!(res.is_some());
            let out = wrapper.entry_remove(&id)?.unwrap();
            assert_eq!(out.entry().id(), &id);
            let res = wrapper.get(&id)?;
            assert!(res.is_none());
        }

        // get latest
        let entries = wrapper
            .store
            .get_many(namespace.id(), Query::all())?
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(entries.len(), 0);

        Ok(())
    }
}
