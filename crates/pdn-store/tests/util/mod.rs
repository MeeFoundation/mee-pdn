#![allow(unused)]

use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock, Weak,
    },
};

use iroh::{
    endpoint::{presets, BindError},
    test_utils::DnsPkarrServer,
    tls::CaTlsConfig,
    Endpoint, EndpointId, RelayMap, RelayMode, SecretKey,
};
use iroh_blobs::store::{GcConfig, ProtectCb, ProtectOutcome};
use iroh_gossip::net::Gossip;
use n0_error::Result;
use pdn_store::{
    api::{Doc, DocsApi},
    engine::{Engine, LiveEvent},
    filter::{serve_whole, SessionAccessProvider},
    protocol::{Docs, DocsDispatch, IdentityResolver},
    Contact, DocTicket, Identity,
};

/// Counts every panic in this process from here on, on any thread: one on a
/// task the runtime catches, such as the blob store's, fails no test by
/// itself. Nextest runs each test in a process of its own.
pub fn count_panics() -> Arc<std::sync::atomic::AtomicUsize> {
    let panics = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = panics.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        previous(info);
    }));
    panics
}

/// One identity per node: these suites reconcile between two bare nodes.
pub const TEST_HOLDER: Identity = Identity::from_bytes([0u8; 32]);
/// A cache big enough that nothing in a scenario evicts.
pub const TEST_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Imports `ticket`'s replica and syncs it with the ticket's nodes, as the
/// data layer joins a store it holds a ticket of.
pub async fn import(docs: &DocsApi, ticket: DocTicket) -> anyhow::Result<Doc> {
    let doc = docs.import_namespace(ticket.capability.clone()).await?;
    doc.start_sync(ticket.contacts(), ticket.identity).await?;
    Ok(doc)
}

/// [`import`], with a subscription opened before the sync starts, so it
/// misses none of the sync's events.
pub async fn import_and_subscribe(
    docs: &DocsApi,
    ticket: DocTicket,
) -> anyhow::Result<(
    Doc,
    impl n0_future::Stream<Item = anyhow::Result<LiveEvent>> + Unpin,
)> {
    let doc = docs.import_namespace(ticket.capability.clone()).await?;
    let events = doc.subscribe().await?;
    doc.start_sync(ticket.contacts(), ticket.identity).await?;
    Ok((doc, events))
}

/// The addresses paired with the one identity these suites run under.
pub fn contacts(addrs: impl IntoIterator<Item = iroh::EndpointAddr>) -> Vec<Contact> {
    addrs
        .into_iter()
        .map(|addr| Contact::new(addr, TEST_HOLDER))
        .collect()
}

pub async fn empty_endpoint() -> Result<Endpoint, BindError> {
    Endpoint::bind(presets::Minimal).await
}

pub async fn endpoint(
    secret_key: SecretKey,
    relay_map: RelayMap,
    dns_pkarr_server: Option<&DnsPkarrServer>,
) -> Result<Endpoint, BindError> {
    let mut builder = Endpoint::builder(presets::Minimal);
    if let Some(dns_pkarr_server) = dns_pkarr_server {
        builder = builder.preset(dns_pkarr_server.preset());
    }

    builder
        .secret_key(secret_key)
        .relay_mode(RelayMode::Custom(relay_map))
        .ca_tls_config(CaTlsConfig::insecure_skip_verify())
        .bind()
        .await
}

/// What a test node's blob protocol records and does: the requests it
/// holds open at once, while held every request kept unanswered, and while
/// ignoring every request arriving left unanswered for good.
#[derive(Debug)]
pub struct BlobServing {
    open: AtomicUsize,
    most: AtomicUsize,
    held: tokio::sync::watch::Sender<bool>,
    ignoring: AtomicBool,
}

impl Default for BlobServing {
    fn default() -> Self {
        Self {
            open: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            held: tokio::sync::watch::channel(false).0,
            ignoring: AtomicBool::new(false),
        }
    }
}

impl BlobServing {
    /// The requests it holds open now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// The most requests it has held open at once.
    pub fn most_open(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    /// Every request from now on, and every one waiting, stays unanswered.
    pub fn hold(&self) {
        self.held.send_replace(true);
    }

    /// Answers the requests held, and every later one.
    pub fn release(&self) {
        self.held.send_replace(false);
    }

    /// Every request arriving from now on stays unanswered for as long as
    /// its connection lives, past [`Self::answer`].
    pub fn ignore(&self) {
        self.ignoring.store(true, Ordering::SeqCst);
    }

    /// Answers every request arriving from now on.
    pub fn answer(&self) {
        self.ignoring.store(false, Ordering::SeqCst);
    }
}

/// `iroh_blobs::provider::handle_connection` with each request counted by
/// [`BlobServing`], held while it holds and ignored while it ignores.
#[derive(Debug, Clone)]
struct ServedBlobs {
    store: iroh_blobs::api::Store,
    serving: Arc<BlobServing>,
}

impl iroh::protocol::ProtocolHandler for ServedBlobs {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        while let Ok(pair) = iroh_blobs::provider::StreamPair::accept(
            &connection,
            iroh_blobs::provider::events::EventSender::DEFAULT,
        )
        .await
        {
            let store = self.store.clone();
            let serving = Arc::clone(&self.serving);
            let connection = connection.clone();
            tokio::spawn(async move {
                let open = serving.open.fetch_add(1, Ordering::SeqCst) + 1;
                serving.most.fetch_max(open, Ordering::SeqCst);
                let mut held = serving.held.subscribe();
                if serving.ignoring.load(Ordering::SeqCst) {
                    let _unanswered = pair;
                    connection.closed().await;
                } else if held.wait_for(|held| !held).await.is_ok() {
                    iroh_blobs::provider::handle_stream(pair, store).await.ok();
                }
                serving.open.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Ok(())
    }
}

/// An iroh node that just has the blobs transport
#[derive(Debug)]
pub struct Node {
    router: iroh::protocol::Router,
    client: Client,
    docs: Docs,
}

impl Deref for Node {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    blobs: iroh_blobs::api::Store,
    docs: pdn_store::api::DocsApi,
}

impl Client {
    fn new(blobs: iroh_blobs::api::Store, docs: pdn_store::api::DocsApi) -> Self {
        Self { blobs, docs }
    }

    pub fn blobs(&self) -> &iroh_blobs::api::Store {
        &self.blobs
    }

    pub fn docs(&self) -> &pdn_store::api::DocsApi {
        &self.docs
    }
}

/// An iroh node builder
#[derive(derive_more::Debug)]
pub struct Builder {
    endpoint: iroh::Endpoint,
    storage: Storage,
    gc_interval: Option<n0_future::time::Duration>,
    #[debug(skip)]
    access: Option<SessionAccessProvider>,
    serve_blobs: bool,
    blob_serving: Option<Arc<BlobServing>>,
    fetches_per_provider: Option<std::num::NonZeroU32>,
}

impl Builder {
    /// Spawns the node
    async fn spawn0(
        self,
        blobs: iroh_blobs::api::Store,
        protected: Option<Arc<OnceLock<Weak<Engine>>>>,
    ) -> anyhow::Result<Node> {
        let mut router = iroh::protocol::Router::builder(self.endpoint.clone());
        let gossip = Gossip::builder().spawn(self.endpoint.clone());
        // These suites reconcile between two bare nodes: one identity each,
        // and unless a scenario names one, a provider that judges nothing —
        // what a consumer states when its scenario is the codec rather than
        // the access model.
        let access = self.access.clone().unwrap_or_else(serve_whole);
        let mut docs_builder = match self.storage {
            Storage::Memory => Docs::memory(TEST_HOLDER, access),
            Storage::Persistent(ref path) => {
                Docs::persistent(path.to_path_buf(), TEST_CACHE_BYTES, TEST_HOLDER, access)
            }
        };
        if let Some(limit) = self.fetches_per_provider {
            docs_builder = docs_builder.fetches_per_provider(limit);
        }
        let docs = match docs_builder
            .spawn(self.endpoint.clone(), blobs.clone(), gossip.clone())
            .await
        {
            Ok(docs) => docs,
            Err(err) => {
                blobs.shutdown().await.ok();
                return Err(err);
            }
        };
        match (self.serve_blobs, self.blob_serving) {
            (false, _) => {}
            (true, Some(serving)) => {
                router = router.accept(
                    iroh_blobs::ALPN,
                    ServedBlobs {
                        store: blobs.clone(),
                        serving,
                    },
                );
            }
            (true, None) => {
                router = router.accept(
                    iroh_blobs::ALPN,
                    iroh_blobs::BlobsProtocol::new(&blobs, None),
                );
            }
        }
        if let Some(protected) = protected {
            protected.set(docs.downgrade()).ok();
        }
        let resolve: IdentityResolver = {
            let docs = docs.clone();
            Arc::new(move |identity| (identity == TEST_HOLDER).then(|| docs.clone()))
        };
        router = router.accept(pdn_store::ALPN, DocsDispatch::new(resolve));
        router = router.accept(iroh_gossip::ALPN, gossip.clone());

        // Build the router
        let router = router.spawn();

        let client = Client::new(blobs.clone(), docs.api().clone());
        Ok(Node {
            router,
            client,
            docs,
        })
    }

    pub fn gc_interval(mut self, value: Option<n0_future::time::Duration>) -> Self {
        self.gc_interval = value;
        self
    }

    /// Mounts no blob transfer: a peer fetching a payload from this node
    /// fails, whatever its blob store holds.
    pub fn without_blob_serving(mut self) -> Self {
        self.serve_blobs = false;
        self
    }

    /// Judges sessions with `provider` in place of [`serve_whole`].
    pub fn access(mut self, provider: SessionAccessProvider) -> Self {
        self.access = Some(provider);
        self
    }

    /// Serves payloads through `serving`, which counts and can hold them.
    pub fn blob_serving(mut self, serving: Arc<BlobServing>) -> Self {
        self.blob_serving = Some(serving);
        self
    }

    /// The engine's limit of fetches in flight toward one provider.
    pub fn fetches_per_provider(mut self, limit: std::num::NonZeroU32) -> Self {
        self.fetches_per_provider = Some(limit);
        self
    }

    fn new(storage: Storage, endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            storage,
            gc_interval: None,
            access: None,
            serve_blobs: true,
            blob_serving: None,
            fetches_per_provider: None,
        }
    }
}

#[derive(Debug)]
enum Storage {
    Memory,
    Persistent(PathBuf),
}

impl Node {
    /// Creates a new node with memory storage
    pub fn memory(endpoint: Endpoint) -> Builder {
        Builder::new(Storage::Memory, endpoint)
    }

    /// Creates a new node with persistent storage
    pub fn persistent(path: impl AsRef<Path>, endpoint: Endpoint) -> Builder {
        Builder::new(Storage::Persistent(path.as_ref().to_owned()), endpoint)
    }
}

impl Builder {
    /// Spawns the node
    pub async fn spawn(self) -> anyhow::Result<Node> {
        let (store, protected) = match self.storage {
            Storage::Memory => {
                let store = iroh_blobs::store::mem::MemStore::new();
                ((*store).clone(), None)
            }
            Storage::Persistent(ref path) => {
                let db_path = path.join("blobs.db");
                let mut opts = iroh_blobs::store::fs::options::Options::new(path);
                let protected = self.gc_interval.map(|interval| {
                    let protected = Arc::default();
                    opts.gc = Some(GcConfig {
                        interval,
                        add_protected: Some(protect_referenced(Arc::clone(&protected))),
                    });
                    protected
                });
                let store = iroh_blobs::store::fs::FsStore::load_with_opts(db_path, opts).await?;
                ((*store).clone(), protected)
            }
        };
        self.spawn0(store, protected).await
    }
}

/// Blob collection's protect callback: every payload the engine in
/// `protected` references, as the data layer protects its node's payloads.
/// Every run before the engine is set is skipped.
fn protect_referenced(protected: Arc<OnceLock<Weak<Engine>>>) -> ProtectCb {
    Arc::new(move |live| {
        let protected = Arc::clone(&protected);
        Box::pin(async move {
            let Some(engine) = protected.get().and_then(Weak::upgrade) else {
                return ProtectOutcome::Abort;
            };
            match engine.collection_hashes().await {
                Ok(hashes) => {
                    live.extend(hashes);
                    ProtectOutcome::Continue
                }
                Err(_) => ProtectOutcome::Abort,
            }
        })
    })
}

impl Node {
    /// The engine under the node's docs, for what its API does not carry.
    #[allow(dead_code)]
    pub fn engine(&self) -> &pdn_store::engine::Engine {
        self.docs.engine()
    }

    /// Returns the node id
    pub fn id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    /// The node's endpoint, for a connection a scenario opens itself.
    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// Ensure the node is "online", aka, is connected to a relay and
    /// has a direct addresses
    pub async fn online(&self) {
        self.router.endpoint().online().await
    }

    /// Shuts down the node
    pub async fn shutdown(self) -> anyhow::Result<()> {
        // The engine first: a running engine starts downloads that read the
        // blob store the router's shutdown closes.
        self.docs.engine().shutdown().await?;
        self.router.shutdown().await?;
        Ok(())
    }

    /// Returns the client
    pub fn client(&self) -> &Client {
        &self.client
    }
}
