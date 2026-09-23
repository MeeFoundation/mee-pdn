//! [`ProtocolHandler`] implementation for the docs [`Engine`].

use std::sync::Arc;

use anyhow::Result;
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint};
use iroh_blobs::api::Store as BlobsStore;
use iroh_gossip::net::Gossip;

use crate::{
    api::DocsApi,
    engine::{DefaultAuthorStorage, Engine, ProtectCallbackHandler},
    net::{accept_session, refuse_session, AbortReason},
    store::Store,
    CapabilityValidator, Identity,
};

#[derive(Default, Debug)]
enum Storage {
    #[default]
    Memory,
    #[cfg(feature = "fs-store")]
    Persistent {
        path: std::path::PathBuf,
        /// What the replica store's cache may hold. Fixed here because a
        /// bound cannot be changed on an open store.
        cache_bytes: usize,
    },
}

/// Docs protocol.
#[derive(Debug, Clone)]
pub struct Docs {
    engine: Arc<Engine>,
    api: DocsApi,
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
            protect_cb: None,
            capability_validator: None,
            rejection_observer: None,
            identity,
            session_access_provider,
            announce_local: crate::engine::announce_nobody(),
            dial_in_process: crate::engine::dial_nobody(),
        }
    }

    /// Create a new [`Builder`] for the docs protocol, using a persistent replica and author storage
    /// in the given directory.
    #[cfg(feature = "fs-store")]
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
            protect_cb: None,
            capability_validator: None,
            rejection_observer: None,
            identity,
            session_access_provider,
            announce_local: crate::engine::announce_nobody(),
            dial_in_process: crate::engine::dial_nobody(),
        }
    }

    /// The engine under this handler, for a node that dispatches accepted
    /// connections to the identity each names.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Creates a new [`Docs`] from an [`Engine`].
    pub fn new(engine: Engine) -> Self {
        let engine = Arc::new(engine);
        let api = DocsApi::spawn(engine.clone());
        Self { engine, api }
    }

    /// Returns the API for this docs instance.
    pub fn api(&self) -> &DocsApi {
        &self.api
    }
}

impl std::ops::Deref for Docs {
    type Target = DocsApi;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

impl ProtocolHandler for Docs {
    /// The one-identity half of the dispatch: this engine answers for its own
    /// identity and refuses every other, which is what a resolver of one
    /// identity does.
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let opening = accept_session(&connection)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?;
        let mine = opening.identity() == self.engine.identity();
        serve_dispatched(connection, opening, mine.then_some(self)).await
    }

    async fn shutdown(&self) {
        if let Err(err) = self.engine.shutdown().await {
            tracing::warn!("shutdown error: {:?}", err);
        }
    }
}

/// Builder for the docs protocol.
#[derive(derive_more::Debug)]
pub struct Builder {
    storage: Storage,
    protect_cb: Option<ProtectCallbackHandler>,
    #[debug("CapabilityValidator")]
    capability_validator: Option<CapabilityValidator>,
    #[debug("RejectionObserver")]
    rejection_observer: Option<crate::RejectionObserver>,
    identity: Identity,
    #[debug("SessionAccessProvider")]
    session_access_provider: crate::filter::SessionAccessProvider,
    #[debug("LocalWriteAnnouncer")]
    announce_local: crate::engine::LocalWriteAnnouncer,
    #[debug("InProcessDialer")]
    dial_in_process: crate::engine::InProcessDialer,
}

impl Builder {
    /// Set the garbage collection protection handler for blobs.
    ///
    /// See [`ProtectCallbackHandler::new`] for details.
    pub fn protect_handler(mut self, protect_handler: ProtectCallbackHandler) -> Self {
        self.protect_cb = Some(protect_handler);
        self
    }

    /// Set a capability validator consulted for every incoming (non-local) entry
    /// before it is persisted. Returning `false` drops the entry.
    ///
    /// This is the injection point for PdnId / UWill capability checks. If
    /// unset, all entries are accepted (vanilla iroh-docs behaviour).
    pub fn capability_validator(mut self, validator: CapabilityValidator) -> Self {
        self.capability_validator = Some(validator);
        self
    }

    /// Set an observer called for every rejection a replica on this node
    /// receives — an own entry a peer refused at its ingest gate, echoed back
    /// in-band on the reconciliation reply.
    ///
    /// If unset, nothing is observed.
    pub fn rejection_observer(mut self, observer: crate::RejectionObserver) -> Self {
        self.rejection_observer = Some(observer);
        self
    }

    /// Set what is told of every local write, with the identity of the
    /// replica that wrote: what reaches the other identities of this node,
    /// which a gossip broadcast never does. Unset, nothing is told.
    pub fn local_write_announcer(mut self, announcer: crate::engine::LocalWriteAnnouncer) -> Self {
        self.announce_local = announcer;
        self
    }

    /// Set what reconciles with a identity of this same node when a contact
    /// names this node's own wire identity. Unset, such a contact is
    /// unreachable.
    pub fn in_process_dialer(mut self, dialer: crate::engine::InProcessDialer) -> Self {
        self.dial_in_process = dialer;
        self
    }

    /// Build a [`Docs`] protocol given a [`BlobsStore`] and [`Gossip`] protocol.
    pub async fn spawn(
        self,
        endpoint: Endpoint,
        blobs: BlobsStore,
        gossip: Gossip,
    ) -> anyhow::Result<Docs> {
        let replica_store = match &self.storage {
            Storage::Memory => Store::memory(),
            #[cfg(feature = "fs-store")]
            Storage::Persistent { path, cache_bytes } => {
                Store::persistent(path.join("docs.redb"), *cache_bytes)?
            }
        };
        let author_store = match &self.storage {
            Storage::Memory => DefaultAuthorStorage::Mem,
            #[cfg(feature = "fs-store")]
            Storage::Persistent { path, .. } => {
                DefaultAuthorStorage::Persistent(path.join("default-author"))
            }
        };
        let downloader = blobs.downloader(&endpoint);
        let engine = Engine::spawn(
            endpoint,
            gossip,
            replica_store,
            blobs,
            downloader,
            author_store,
            self.protect_cb,
            self.capability_validator,
            self.rejection_observer,
            self.session_access_provider,
            self.identity,
            self.announce_local,
            self.dial_in_process,
        )
        .await?;
        Ok(Docs::new(engine))
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
    /// Dispatch by `resolve`, which answers with the engine of a identity
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
/// no accept path waits on the wire inside the actor loop.
async fn serve_dispatched(
    connection: Connection,
    opening: crate::net::SessionOpening<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    docs: Option<&Docs>,
) -> Result<(), iroh::protocol::AcceptError> {
    match docs {
        Some(docs) => docs
            .engine
            .handle_session(connection, opening)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?,
        // Byte-identical to the refusal a replica this node does not
        // hold draws, so naming a identity tells a caller nothing.
        None => refuse_session(opening, AbortReason::NotFound)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?,
    }
    Ok(())
}
