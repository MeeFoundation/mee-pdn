//! The docs [`Engine`]'s assembly, and the [`ProtocolHandler`] that hands an
//! accepted session to the engine of the identity it names.

use std::sync::{Arc, Weak};

use anyhow::Result;
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint};
use iroh_blobs::api::Store as BlobsStore;
use iroh_gossip::net::Gossip;

use crate::{
    api::DocsApi,
    engine::{DefaultAuthorStorage, Engine},
    net::{accept_session, refuse_session, AbortReason},
    store::Store,
    Identity,
};

#[derive(Default, Debug)]
enum Storage {
    #[default]
    Memory,
    Persistent {
        path: std::path::PathBuf,
        /// What the replica store's cache may hold. Fixed here because a
        /// bound cannot be changed on an open store.
        cache_bytes: usize,
    },
}

/// Docs protocol.
///
/// Its clones hold the engine's only strong references: a [`DocsApi`] or a
/// [`Doc`](crate::api::Doc) can sit inside what the engine holds, so the
/// engine drops with the last `Docs`, whatever handles remain.
#[derive(Debug, Clone)]
pub struct Docs {
    engine: Arc<Engine>,
    api: DocsApi,
    replica_cache_bytes: Option<usize>,
}

impl Docs {
    /// Create a new [`Builder`] for the docs protocol, using in memory replica and author storage.
    ///
    /// `identity` is whom every replica of this engine is held for, and
    /// `session_access_provider` what judges its sessions: an assembly
    /// that states neither cannot serve one.
    pub fn memory(
        identity: Identity,
        session_access_provider: crate::filter::SessionAccessProvider,
    ) -> Builder {
        Builder {
            storage: Storage::Memory,
            identity,
            session_access_provider,
            co_located: None,
            fetches_per_provider: FETCHES_PER_PROVIDER,
        }
    }

    /// Create a new [`Builder`] for the docs protocol, using a persistent replica and author storage
    /// in the given directory.
    ///
    /// `cache_bytes` caps what this store's cache holds; the bound cannot
    /// be changed once the store is open, so a node dividing one budget
    /// among several identities cuts the share before opening any of them.
    pub fn persistent(
        path: std::path::PathBuf,
        cache_bytes: usize,
        identity: Identity,
        session_access_provider: crate::filter::SessionAccessProvider,
    ) -> Builder {
        Builder {
            storage: Storage::Persistent { path, cache_bytes },
            identity,
            session_access_provider,
            co_located: None,
            fetches_per_provider: FETCHES_PER_PROVIDER,
        }
    }

    /// The engine these docs hold, for a node that dispatches accepted
    /// connections to the identity each names.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// A reference to the engine that does not keep it alive.
    pub fn downgrade(&self) -> Weak<Engine> {
        Arc::downgrade(&self.engine)
    }

    /// Creates a new [`Docs`] from an [`Engine`].
    pub fn new(engine: Engine) -> Self {
        let engine = Arc::new(engine);
        Self {
            api: DocsApi::new(&engine),
            engine,
            replica_cache_bytes: None,
        }
    }

    /// Returns the API for this docs instance.
    pub fn api(&self) -> &DocsApi {
        &self.api
    }

    /// The cache bound the replica store was opened with; `None` for a
    /// store in memory, or for docs built with [`Docs::new`] from an engine.
    pub fn replica_cache_bytes(&self) -> Option<usize> {
        self.replica_cache_bytes
    }
}

impl std::ops::Deref for Docs {
    type Target = DocsApi;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

/// Builder for the docs protocol.
#[derive(derive_more::Debug)]
pub struct Builder {
    storage: Storage,
    identity: Identity,
    #[debug("SessionAccessProvider")]
    session_access_provider: crate::filter::SessionAccessProvider,
    co_located: Option<crate::engine::CoLocatedRequests>,
    fetches_per_provider: u32,
}

/// The fetches in flight toward one provider unless the builder names
/// another: the bidirectional streams the transport grants a connection by
/// default, one per request.
pub const FETCHES_PER_PROVIDER: u32 = 100;

impl Builder {
    /// Set where this engine sends what it asks of its node about the
    /// node's other identities: a local write to announce, a contact naming
    /// this node to reconcile inside the process. Unset, both reach nobody.
    pub fn co_located_requests(mut self, requests: crate::engine::CoLocatedRequests) -> Self {
        self.co_located = Some(requests);
        self
    }

    /// Set how many payloads this engine fetches from one provider at once:
    /// the bidirectional streams the endpoint grants a connection, past
    /// which a request only waits inside the transport.
    pub fn fetches_per_provider(mut self, limit: u32) -> Self {
        self.fetches_per_provider = limit;
        self
    }

    /// Build a [`Docs`] protocol given a [`BlobsStore`] and [`Gossip`] protocol.
    pub async fn spawn(
        self,
        endpoint: Endpoint,
        blobs: BlobsStore,
        gossip: Gossip,
    ) -> anyhow::Result<Docs> {
        let (replica_store, replica_cache_bytes) = match &self.storage {
            Storage::Memory => (Store::memory(), None),
            Storage::Persistent { path, cache_bytes } => (
                Store::persistent(path.join("docs.redb"), *cache_bytes)?,
                Some(*cache_bytes),
            ),
        };
        let author_store = match &self.storage {
            Storage::Memory => DefaultAuthorStorage::Mem,
            Storage::Persistent { path, .. } => {
                DefaultAuthorStorage::Persistent(path.join("default-author"))
            }
        };
        let engine = Engine::spawn(
            endpoint,
            gossip,
            replica_store,
            blobs,
            usize::try_from(self.fetches_per_provider).unwrap_or(usize::MAX),
            author_store,
            self.session_access_provider,
            self.identity,
            self.co_located,
        )
        .await?;
        let mut docs = Docs::new(engine);
        docs.replica_cache_bytes = replica_cache_bytes;
        Ok(docs)
    }
}

/// Resolves the identity an accepted session names to the engine that holds
/// its replicas. `None` refuses the session as not hosted.
pub type IdentityResolver = Arc<dyn Fn(Identity) -> Option<Docs> + Send + Sync + 'static>;

/// The docs handler of a node hosting several identities: it reads an
/// accepted connection's first message and hands the session to the
/// engine of the identity that message names, before any replica is touched
/// (ADR-0013).
#[derive(derive_more::Debug, Clone)]
pub struct DocsDispatch {
    #[debug("IdentityResolver")]
    resolve: IdentityResolver,
}

impl DocsDispatch {
    /// Dispatch by `resolve`, which answers with the engine of an identity
    /// this node hosts.
    pub fn new(resolve: IdentityResolver) -> Self {
        Self { resolve }
    }
}

impl ProtocolHandler for DocsDispatch {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let opening = accept_session(&connection)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?;
        let resolved = (self.resolve)(opening.identity());
        serve_dispatched(connection, opening, resolved.as_ref()).await
    }
}

/// Hand a read session to the engine that answers for the identity it names,
/// or refuse it. The only way a session enters an engine: reading the first
/// message is the caller's, so nothing hands an engine a raw connection and
/// no accept path waits on the wire inside the actor loop. An engine of
/// another identity is refused as none: its book would judge a session
/// addressed to a replica it does not answer for.
async fn serve_dispatched(
    connection: Connection,
    opening: crate::net::SessionOpening<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    docs: Option<&Docs>,
) -> Result<(), iroh::protocol::AcceptError> {
    match docs.filter(|docs| docs.engine.identity() == opening.identity()) {
        Some(docs) => docs
            .engine
            .handle_session(connection, opening)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?,
        // Byte-identical to the refusal a replica this node does not
        // hold draws, so naming an identity tells a caller nothing.
        None => refuse_session(opening, AbortReason::NotFound)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?,
    }
    Ok(())
}
