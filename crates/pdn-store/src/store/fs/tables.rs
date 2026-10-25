#![allow(missing_docs)]
// Table Definitions

use bytes::Bytes;
use n0_future::time::Instant;
use redb::{
    MultimapTable, MultimapTableDefinition, ReadOnlyTable, ReadTransaction, Table, TableDefinition,
    WriteTransaction,
};

use crate::PeerIdBytes;

/// Table: Authors
/// Key:   `[u8; 32]` # AuthorId
/// Value: `[u8; 32]` # Author
pub(crate) const AUTHORS_TABLE: TableDefinition<&[u8; 32], &[u8; 32]> =
    TableDefinition::new("authors-1");

/// Table: Namespaces v2
/// Key:   `[u8; 32]`       # NamespaceId
/// Value: `(u8, [u8; 32])` # (CapabilityKind, Capability)
pub(crate) const NAMESPACES_TABLE: TableDefinition<&[u8; 32], (u8, &[u8; 32])> =
    TableDefinition::new("namespaces-2");

/// Table: Records
/// Key:   `([u8; 32], [u8; 32], &[u8])`
///      # (NamespaceId, AuthorId, Key)
/// Value: `(u64, [u8; 64], [u8; 64], u64, [u8; 32])`
///      # (timestamp, signature_namespace, signature_author, len, hash)
pub(crate) const RECORDS_TABLE: TableDefinition<RecordsId, RecordsValue> =
    TableDefinition::new("records-1");
pub(crate) type RecordsId<'a> = (&'a [u8; 32], &'a [u8; 32], &'a [u8]);
pub(crate) type RecordsIdOwned = ([u8; 32], [u8; 32], Bytes);
pub(crate) type RecordsValue<'a> = (u64, &'a [u8; 64], &'a [u8; 64], u64, &'a [u8; 32]);
pub(crate) type RecordsTable = ReadOnlyTable<RecordsId<'static>, RecordsValue<'static>>;

/// Table: Latest per author
/// Key:   `([u8; 32], [u8; 32])`    # (NamespaceId, AuthorId)
/// Value: `(u64, Vec<u8>)`          # (Timestamp, Key)
pub(crate) const LATEST_PER_AUTHOR_TABLE: TableDefinition<
    LatestPerAuthorKey,
    LatestPerAuthorValue,
> = TableDefinition::new("latest-by-author-1");
pub(crate) type LatestPerAuthorKey<'a> = (&'a [u8; 32], &'a [u8; 32]);
pub(crate) type LatestPerAuthorValue<'a> = (u64, &'a [u8]);

/// Table: Records by key
/// Key:   `([u8; 32], Vec<u8>, [u8; 32])` # (NamespaceId, Key, AuthorId)
/// Value: `()`
pub(crate) const RECORDS_BY_KEY_TABLE: TableDefinition<RecordsByKeyId, ()> =
    TableDefinition::new("records-by-key-1");
pub(crate) type RecordsByKeyId<'a> = (&'a [u8; 32], &'a [u8], &'a [u8; 32]);
pub(crate) type RecordsByKeyIdOwned = ([u8; 32], Bytes, [u8; 32]);

/// Table: Peers per document.
/// Key:   `[u8; 32]`        # NamespaceId
/// Value: `(u64, [u8; 32])` # ([`Nanos`], &[`PeerIdBytes`]) representing the last time a peer was used.
pub(crate) const NAMESPACE_PEERS_TABLE: MultimapTableDefinition<&[u8; 32], (Nanos, &PeerIdBytes)> =
    MultimapTableDefinition::new("sync-peers-1");
/// Number of nanoseconds elapsed since [`n0_future::time::SystemTime::UNIX_EPOCH`]. Used to register the
/// last time a peer was useful in a document.
// NOTE: resolution is nanoseconds, stored as a u64 since this covers ~500years from unix epoch,
// which should be more than enough
pub(crate) type Nanos = u64;

self_cell::self_cell! {
    struct TransactionAndTablesInner {
        owner: WriteTransaction,
        #[covariant]
        dependent: Tables,
    }
}

#[derive(derive_more::Debug)]
pub(crate) struct TransactionAndTables {
    #[debug("TransactionAndTablesInner")]
    inner: TransactionAndTablesInner,
    pub(crate) since: Instant,
    dirty: bool,
}

impl TransactionAndTables {
    pub fn new(tx: WriteTransaction) -> std::result::Result<Self, redb::TableError> {
        Ok(Self {
            inner: TransactionAndTablesInner::try_new(tx, |tx| Tables::new(tx))?,
            since: Instant::now(),
            dirty: false,
        })
    }

    pub fn tables(&self) -> &Tables<'_> {
        self.inner.borrow_dependent()
    }

    /// Whether anything was written through this transaction.
    ///
    /// Mutable access is the only way to write and it runs through
    /// `with_tables_mut`, so this cannot miss a write; it does count a
    /// closure that took mutable access and wrote nothing.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn with_tables_mut<T>(
        &mut self,
        f: impl FnOnce(&mut Tables) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.dirty = true;
        self.inner.with_dependent_mut(|_, t| f(t))
    }

    /// Commits, syncing the file when `synced`; answers whether it did. An
    /// unsynced commit is visible to every later reader and reaches the disk
    /// with the next synced one.
    pub fn commit(self, synced: bool) -> std::result::Result<bool, redb::CommitError> {
        let mut tx = self.inner.into_owner();
        // Refused only after a persistent savepoint, which this store never takes.
        let synced = synced || tx.set_durability(redb::Durability::None).is_err();
        tx.commit()?;
        Ok(synced)
    }
}

#[derive(derive_more::Debug)]
pub(crate) struct Tables<'tx> {
    pub records: Table<'tx, RecordsId<'static>, RecordsValue<'static>>,
    pub(crate) records_by_key: Table<'tx, RecordsByKeyId<'static>, ()>,
    pub namespaces: Table<'tx, &'static [u8; 32], (u8, &'static [u8; 32])>,
    pub(crate) latest_per_author:
        Table<'tx, LatestPerAuthorKey<'static>, LatestPerAuthorValue<'static>>,
    #[debug("MultimapTable")]
    pub(crate) namespace_peers:
        MultimapTable<'tx, &'static [u8; 32], (Nanos, &'static PeerIdBytes)>,
    pub authors: Table<'tx, &'static [u8; 32], &'static [u8; 32]>,
}

impl<'tx> Tables<'tx> {
    pub fn new(tx: &'tx WriteTransaction) -> Result<Self, redb::TableError> {
        let records = tx.open_table(RECORDS_TABLE)?;
        let records_by_key = tx.open_table(RECORDS_BY_KEY_TABLE)?;
        let namespaces = tx.open_table(NAMESPACES_TABLE)?;
        let latest_per_author = tx.open_table(LATEST_PER_AUTHOR_TABLE)?;
        let namespace_peers = tx.open_multimap_table(NAMESPACE_PEERS_TABLE)?;
        let authors = tx.open_table(AUTHORS_TABLE)?;
        Ok(Self {
            records,
            records_by_key,
            namespaces,
            latest_per_author,
            namespace_peers,
            authors,
        })
    }
}
#[derive(derive_more::Debug)]
pub(crate) struct ReadOnlyTables {
    pub records: ReadOnlyTable<RecordsId<'static>, RecordsValue<'static>>,
    pub(crate) records_by_key: ReadOnlyTable<RecordsByKeyId<'static>, ()>,
    pub namespaces: ReadOnlyTable<&'static [u8; 32], (u8, &'static [u8; 32])>,
}

impl ReadOnlyTables {
    pub fn new(tx: ReadTransaction) -> Result<Self, redb::TableError> {
        let records = tx.open_table(RECORDS_TABLE)?;
        let records_by_key = tx.open_table(RECORDS_BY_KEY_TABLE)?;
        let namespaces = tx.open_table(NAMESPACES_TABLE)?;
        Ok(Self {
            records,
            records_by_key,
            namespaces,
        })
    }
}
