//! Storage trait and implementation for pdn-store documents
use std::num::NonZeroUsize;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::AuthorId;

pub mod fs;
mod pubkeys;
mod util;
pub(crate) use fs::Store;
pub(crate) use pubkeys::*;

/// Number of peers to cache per document.
pub(crate) const PEERS_PER_DOC_CACHE_SIZE: NonZeroUsize = match NonZeroUsize::new(5) {
    Some(val) => val,
    None => panic!("this is clearly non zero"),
};

/// Error return from [`Store::open_replica`]
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    /// The replica does not exist.
    #[error("Replica not found")]
    NotFound,
    /// Other error while opening the replica.
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Outcome of [`Store::import_namespace`]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ImportNamespaceOutcome {
    /// The namespace did not exist before and is now inserted.
    Inserted,
    /// The namespace existed and its capability remains unchanged.
    NoChange,
}

/// A query builder for document queries.
#[derive(Debug, Default)]
pub struct QueryBuilder<K> {
    kind: K,
    filter_author: AuthorFilter,
    filter_key: KeyFilter,
    limit: Option<u64>,
    include_empty: bool,
}

impl<K> QueryBuilder<K> {
    /// Call to include empty entries (deletion markers).
    pub fn include_empty(mut self) -> Self {
        self.include_empty = true;
        self
    }
    /// Filter by exact key match.
    pub fn key_exact(mut self, key: impl AsRef<[u8]>) -> Self {
        self.filter_key = KeyFilter::Exact(key.as_ref().to_vec().into());
        self
    }
    /// Filter by key prefix.
    pub fn key_prefix(mut self, key: impl AsRef<[u8]>) -> Self {
        self.filter_key = KeyFilter::Prefix(key.as_ref().to_vec().into());
        self
    }
    /// Filter by author.
    pub fn author(mut self, author: AuthorId) -> Self {
        self.filter_author = AuthorFilter::Exact(author);
        self
    }
    /// Set the maximum number of entries to be returned.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Query on all entries without aggregation, ordered by author, then key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlatQuery {}

/// Query that only returns the latest entry for a key which has entries from multiple authors,
/// ordered by key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SingleLatestPerKeyQuery {}

impl QueryBuilder<FlatQuery> {
    /// Build the query.
    pub fn build(self) -> Query {
        Query::from(self)
    }
}

impl QueryBuilder<SingleLatestPerKeyQuery> {
    /// Build the query.
    pub fn build(self) -> Query {
        Query::from(self)
    }
}

impl From<QueryBuilder<SingleLatestPerKeyQuery>> for Query {
    fn from(builder: QueryBuilder<SingleLatestPerKeyQuery>) -> Query {
        Query {
            kind: QueryKind::SingleLatestPerKey(builder.kind),
            filter_author: builder.filter_author,
            filter_key: builder.filter_key,
            limit: builder.limit,
            include_empty: builder.include_empty,
        }
    }
}

impl From<QueryBuilder<FlatQuery>> for Query {
    fn from(builder: QueryBuilder<FlatQuery>) -> Query {
        Query {
            kind: QueryKind::Flat(builder.kind),
            filter_author: builder.filter_author,
            filter_key: builder.filter_key,
            limit: builder.limit,
            include_empty: builder.include_empty,
        }
    }
}

/// Note: When using the `SingleLatestPerKey` query kind, the key filter is applied *before* the
/// grouping, the author filter is applied *after* the grouping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    kind: QueryKind,
    filter_author: AuthorFilter,
    filter_key: KeyFilter,
    limit: Option<u64>,
    include_empty: bool,
}

impl Query {
    /// Query all records.
    pub fn all() -> QueryBuilder<FlatQuery> {
        Default::default()
    }
    /// Query only the latest entry for each key, omitting older entries if the entry was written
    /// to by multiple authors.
    pub fn single_latest_per_key() -> QueryBuilder<SingleLatestPerKeyQuery> {
        Default::default()
    }

    /// Create a [`Query::all`] query filtered by a single author.
    pub fn author(author: AuthorId) -> QueryBuilder<FlatQuery> {
        Self::all().author(author)
    }

    /// Create a [`Query::all`] query filtered by a single key.
    pub fn key_exact(key: impl AsRef<[u8]>) -> QueryBuilder<FlatQuery> {
        Self::all().key_exact(key)
    }

    /// Create a [`Query::all`] query filtered by a key prefix.
    pub fn key_prefix(prefix: impl AsRef<[u8]>) -> QueryBuilder<FlatQuery> {
        Self::all().key_prefix(prefix)
    }

    /// Get the limit for this query (max. number of entries to emit).
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }
}

#[derive(derive_more::Debug, Clone, Serialize, Deserialize)]
enum QueryKind {
    #[debug("Flat")]
    Flat(FlatQuery),
    #[debug("SingleLatestPerKey")]
    SingleLatestPerKey(SingleLatestPerKeyQuery),
}

/// Key matching.
#[derive(Debug, Serialize, Deserialize, Clone, Default, Eq, PartialEq)]
pub(crate) enum KeyFilter {
    /// Matches any key.
    #[default]
    Any,
    /// Only keys that are exactly the provided value.
    Exact(Bytes),
    /// All keys that start with the provided value.
    Prefix(Bytes),
}

impl<T: AsRef<[u8]>> From<T> for KeyFilter {
    fn from(value: T) -> Self {
        KeyFilter::Exact(Bytes::copy_from_slice(value.as_ref()))
    }
}

impl KeyFilter {
    /// Test if a key is matched by this [`KeyFilter`].
    pub fn matches(&self, key: &[u8]) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(k) => &k[..] == key,
            Self::Prefix(p) => key.starts_with(p),
        }
    }
}

/// Author matching.
#[derive(Debug, Serialize, Deserialize, Clone, Default, Eq, PartialEq)]
pub(crate) enum AuthorFilter {
    /// Matches any author.
    #[default]
    Any,
    /// Matches exactly the provided author.
    Exact(AuthorId),
}

impl AuthorFilter {
    /// Test if an author is matched by this [`AuthorFilter`].
    pub fn matches(&self, author: &AuthorId) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(a) => a == author,
        }
    }
}

impl From<AuthorId> for AuthorFilter {
    fn from(value: AuthorId) -> Self {
        AuthorFilter::Exact(value)
    }
}
