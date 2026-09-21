//! Session-scoped egress access control for reconciliation.
//!
//! The reconciliation engine reads entries only through the ranger store
//! trait: the crate-internal `SessionStore` wraps a replica's store with an optional per-session
//! [`EntryFilter`], so every read the engine performs — the first key, range
//! iterations, fingerprints, counts — sees only admitted entries. Filtering
//! the iterators filters the fingerprints by construction (fingerprints are
//! computed by iterating ranges), so no unadmitted entry can leak through a
//! fingerprint, a split boundary, or an item transmission. Write-side
//! methods (`entry_put`, `prefixes_of`, `remove_prefix_filtered`) pass
//! through unfiltered — ingest is the `validate_entry` hook's concern.
//!
//! The domain meaning of a filter (grants, identities) stays outside this
//! crate: an embedder hands in opaque predicates over [`SignedEntry`]
//! through a [`SessionAccessProvider`], consulted per session on both
//! session roles — accepting an incoming sync request and dialing out —
//! because both ends of a reconciliation serve entries. The provider sees
//! the [`Holder`] whose replica the session addresses and the one its
//! caller acts for, so a node hosting several holders judges each session
//! by the one named in it.

use std::{future::Future, pin::Pin, sync::Arc};

use iroh::PublicKey;

use crate::{
    holder::Holder,
    keys::NamespaceId,
    ranger::{Fingerprint, Range, RangeEntry, Store, ValidateOutcome},
    store::PublicKeyStore,
    sync::{RecordIdentifier, SignedEntry},
};

/// Per-session egress predicate: `true` admits the entry into the peer's
/// view. Must be cheap — it runs on every entry a range scan touches.
pub type EntryFilter = Arc<dyn Fn(&SignedEntry) -> bool + Send + Sync + 'static>;

/// Per-session ingest verdict on an entry the peer offers. Runs on every
/// entry a session carries, so it must be cheap.
///
/// It rides the session the way [`EntryFilter`] does, which is what makes
/// a write set frozen at setup hold for that session and no longer: a
/// verdict kept beside the session instead would outlive it and admit,
/// under a grant already withdrawn, what a later session offers.
pub type SessionIngest = Arc<dyn Fn(&SignedEntry) -> ValidateOutcome + Send + Sync + 'static>;

/// What one session may see of a replica and what it may put into it,
/// decided at session setup and frozen for the session.
#[derive(Clone)]
pub enum SessionAccess {
    /// The session proceeds. `egress` narrows what this side reveals —
    /// `None` reveals the replica whole — and `ingest` judges every entry
    /// the peer offers; `None` leaves that to the consumer's validator.
    Allow {
        /// What this side reveals; `None` reveals the replica whole.
        egress: Option<EntryFilter>,
        /// What this side admits; `None` leaves it to the validator.
        ingest: Option<SessionIngest>,
    },
    /// No session: reject as if the replica were not hosted here.
    Deny,
}

impl SessionAccess {
    /// The whole replica out, and the consumer's validator alone in.
    pub fn whole() -> Self {
        Self::Allow {
            egress: None,
            ingest: None,
        }
    }
}

impl std::fmt::Debug for SessionAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionAccess::Allow { egress, ingest } => f
                .debug_struct("Allow")
                .field("egress", &egress.as_ref().map(|_| "filtered"))
                .field("ingest", &ingest.as_ref().map(|_| "judged"))
                .finish(),
            SessionAccess::Deny => write!(f, "Deny"),
        }
    }
}

/// Which end of the session this node is on when the provider is asked.
///
/// Both ends serve entries (reconciliation is bidirectional), but the
/// policies differ: a node may refuse to *accept* a caller it cannot judge
/// while still being allowed to *dial* out with a closed egress (serving
/// nothing, receiving whatever the callee's own filter admits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// This node is accepting an incoming sync request.
    Accept,
    /// This node is dialing out to sync.
    Dial,
}

/// Future returned by a [`SessionAccessProvider`].
pub type SessionAccessFuture = Pin<Box<dyn Future<Output = SessionAccess> + Send + 'static>>;

/// Decides, per session, what a peer may see of a namespace. Consulted on
/// both session roles, with the [`Holder`] whose replica the session
/// addresses and the one its caller acts for beside the peer's node id:
/// the verdict follows those two alone and is never widened by another
/// holder the same node id resolves to.
///
/// Required of every assembly: without one a consumer would serve
/// sessions it never judged. [`serve_whole`] is what a suite names when it
/// wants upstream's unjudged behaviour.
pub type SessionAccessProvider = Arc<
    dyn Fn(NamespaceId, Holder, Holder, PublicKey, SessionRole) -> SessionAccessFuture
        + Send
        + Sync
        + 'static,
>;

/// A provider that judges nothing: every session sees the replica whole.
/// Named rather than defaulted, so an assembly that wants it says so.
pub fn serve_whole() -> SessionAccessProvider {
    Arc::new(|_namespace, _holder, _caller, _peer, _role| {
        Box::pin(std::future::ready(SessionAccess::whole()))
    })
}

/// A replica store narrowed to one session's view.
///
/// With `filter: None` every method delegates unchanged. With a filter, all
/// reading methods yield only admitted entries; `get_first` returns the
/// first *admitted* key (the first physical key would leak an unadmitted
/// entry's existence through the initial range boundary), and
/// `get_fingerprint` recomputes over the filtered range iterator.
pub(crate) struct SessionStore<S> {
    inner: S,
    filter: Option<EntryFilter>,
}

impl<S> SessionStore<S> {
    pub(crate) fn new(inner: S, filter: Option<EntryFilter>) -> Self {
        Self { inner, filter }
    }
}

/// Whether `entry` passes the session's filter (no filter admits all).
fn admitted(filter: &Option<EntryFilter>, entry: &SignedEntry) -> bool {
    match filter {
        None => true,
        Some(f) => f(entry),
    }
}

impl<S: Store<SignedEntry>> Store<SignedEntry> for SessionStore<S> {
    type Error = S::Error;
    type RangeIterator<'a>
        = FilteredIter<S::RangeIterator<'a>>
    where
        S: 'a;
    type ParentIterator<'a>
        = S::ParentIterator<'a>
    where
        S: 'a;

    fn get_first(&mut self) -> Result<RecordIdentifier, Self::Error> {
        let Some(filter) = self.filter.clone() else {
            return self.inner.get_first();
        };
        // The full range (x == y wraps around); the first admitted key, or
        // the default when nothing is admitted — indistinguishable from an
        // empty replica, exactly as intended.
        let all = Range::new(RecordIdentifier::default(), RecordIdentifier::default());
        for entry in self.inner.get_range(all)? {
            let entry = entry?;
            if filter(&entry) {
                return Ok(entry.id().clone());
            }
        }
        Ok(RecordIdentifier::default())
    }

    #[cfg(test)]
    fn get(&mut self, key: &RecordIdentifier) -> Result<Option<SignedEntry>, Self::Error> {
        let found = self.inner.get(key)?;
        Ok(found.filter(|e| admitted(&self.filter, e)))
    }

    #[cfg(test)]
    fn len(&mut self) -> Result<usize, Self::Error> {
        let all = Range::new(RecordIdentifier::default(), RecordIdentifier::default());
        self.get_range_len(all)
    }

    #[cfg(test)]
    fn is_empty(&mut self) -> Result<bool, Self::Error> {
        Ok(self.len()? == 0)
    }

    fn get_fingerprint(
        &mut self,
        range: &Range<RecordIdentifier>,
    ) -> Result<Fingerprint, Self::Error> {
        if self.filter.is_none() {
            return self.inner.get_fingerprint(range);
        }
        // Recomputed over the session's own (filtered) range iterator, the
        // same way the store computes it over its full one.
        let mut fp = Fingerprint::empty();
        for entry in self.get_range(range.clone())? {
            fp ^= entry?.as_fingerprint();
        }
        Ok(fp)
    }

    fn entry_put(&mut self, entry: SignedEntry) -> Result<(), Self::Error> {
        self.inner.entry_put(entry)
    }

    fn get_range(
        &mut self,
        range: Range<RecordIdentifier>,
    ) -> Result<Self::RangeIterator<'_>, Self::Error> {
        Ok(FilteredIter {
            inner: self.inner.get_range(range)?,
            filter: self.filter.clone(),
        })
    }

    #[cfg(test)]
    fn prefixed_by(
        &mut self,
        prefix: &RecordIdentifier,
    ) -> Result<Self::RangeIterator<'_>, Self::Error> {
        Ok(FilteredIter {
            inner: self.inner.prefixed_by(prefix)?,
            filter: self.filter.clone(),
        })
    }

    fn prefixes_of(
        &mut self,
        key: &RecordIdentifier,
    ) -> Result<Self::ParentIterator<'_>, Self::Error> {
        self.inner.prefixes_of(key)
    }

    #[cfg(test)]
    fn all(&mut self) -> Result<Self::RangeIterator<'_>, Self::Error> {
        Ok(FilteredIter {
            inner: self.inner.all()?,
            filter: self.filter.clone(),
        })
    }

    #[cfg(test)]
    fn entry_remove(&mut self, key: &RecordIdentifier) -> Result<Option<SignedEntry>, Self::Error> {
        self.inner.entry_remove(key)
    }

    fn remove_prefix_filtered(
        &mut self,
        prefix: &RecordIdentifier,
        predicate: impl Fn(&crate::sync::Record) -> bool,
    ) -> Result<usize, Self::Error> {
        self.inner.remove_prefix_filtered(prefix, predicate)
    }
}

impl<S: PublicKeyStore> PublicKeyStore for SessionStore<S> {
    fn public_key(&self, id: &[u8; 32]) -> Result<PublicKey, iroh::KeyParsingError> {
        self.inner.public_key(id)
    }
}

/// A range iterator narrowed to admitted entries; errors pass through.
pub(crate) struct FilteredIter<I> {
    inner: I,
    filter: Option<EntryFilter>,
}

impl<I, E> Iterator for FilteredIter<I>
where
    I: Iterator<Item = Result<SignedEntry, E>>,
{
    type Item = Result<SignedEntry, E>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next()? {
                Err(e) => return Some(Err(e)),
                Ok(entry) => {
                    if admitted(&self.filter, &entry) {
                        return Some(Ok(entry));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_access_debug_is_opaque() {
        let access = SessionAccess::Allow {
            egress: Some(Arc::new(|_| true)),
            ingest: None,
        };
        assert_eq!(
            format!("{access:?}"),
            r#"Allow { egress: Some("filtered"), ingest: None }"#
        );
    }
}
