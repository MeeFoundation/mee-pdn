pub mod gossip;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_helpers;

use futures_core::Stream;
use futures_util::stream::{BoxStream, StreamExt as _};
#[cfg(feature = "mdns")]
use iroh::address_lookup::MdnsAddressLookup;
use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    Endpoint, EndpointAddr as IrohNodeAddr, EndpointId as IrohNodeId, RelayMode, RelayUrl,
    TransportAddr,
};
use iroh_willow::proto::data_model::PathExt;
use iroh_willow::{engine::AcceptOpts, Engine as WillowEngine, ALPN};
use mee_sync_api as api;
use mee_sync_api::SyncError;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// ALPN for the mee-connect handshake protocol.
pub const MEE_CONNECT_ALPN: &[u8] = b"mee-connect/0";

/// Controls which discovery services the sync engine uses.
pub struct DiscoveryConfig {
    /// Relay mode: Disabled, Default (n0 servers), or Custom.
    pub relay_mode: RelayMode,
    /// Enable mDNS for local network peer discovery.
    pub mdns: bool,
    /// Enable pkarr publishing + DNS lookup (n0 infrastructure).
    pub n0_discovery: bool,
    /// Bind to a specific address (None = OS default).
    pub bind_addr: Option<SocketAddr>,
    /// Clear all IP transports before binding. Required for test
    /// stability on multi-homed machines (prevents multipath flakiness).
    pub clear_ip_transports: bool,
    /// Gossip discovery config. None = gossip disabled.
    pub gossip: Option<gossip::GossipConfig>,
}

impl DiscoveryConfig {
    /// No discovery, no relay. Current localhost-only behavior.
    pub fn disabled() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: false,
            n0_discovery: false,
            bind_addr: None,
            clear_ip_transports: false,
            gossip: None,
        }
    }

    /// mDNS only — discovers peers on the local network, no relay.
    pub fn local() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: true,
            n0_discovery: false,
            bind_addr: None,
            clear_ip_transports: false,
            gossip: None,
        }
    }

    /// Full discovery: relay + DNS/pkarr + mDNS.
    pub fn full() -> Self {
        Self {
            relay_mode: RelayMode::Default,
            mdns: true,
            n0_discovery: true,
            bind_addr: None,
            clear_ip_transports: false,
            gossip: None,
        }
    }

    /// Test-stable config: no discovery, localhost-only binding,
    /// cleared IP transports to prevent multipath flakiness.
    pub fn test() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: false,
            n0_discovery: false,
            bind_addr: Some(SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                0u16,
            )),
            clear_ip_transports: true,
            gossip: None,
        }
    }
}

// -- Connect protocol types ------------------------------------------------

#[derive(Serialize, Deserialize)]
struct ConnectRequest {
    ticket: api::SyncTicket,
}

#[derive(Serialize, Deserialize)]
struct ConnectResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// -- ConnectHandler (accept side) ------------------------------------------

#[derive(Clone)]
struct ConnectHandler {
    client: iroh_willow::rpc::client::MemClient,
    imported_namespaces: Arc<Mutex<HashSet<api::NamespaceId>>>,
}

impl std::fmt::Debug for ConnectHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectHandler").finish_non_exhaustive()
    }
}

#[allow(clippy::expect_used)]
impl ProtocolHandler for ConnectHandler {
    fn accept(
        &self,
        conn: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let this = self.clone();
        async move {
            let result = this.handle_connect(conn).await;
            result.map_err(|e| AcceptError::from_err(std::io::Error::other(e)))
        }
    }
}

#[allow(clippy::expect_used, clippy::as_conversions)]
impl ConnectHandler {
    async fn handle_connect(&self, conn: Connection) -> Result<(), SyncError> {
        let (mut send, mut recv) = conn
            .accept_bi()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        // Read length-prefixed JSON request
        let len = recv
            .read_u32()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let mut buf = vec![0u8; len as usize];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        let req: ConnectRequest = serde_json::from_slice(&buf)
            .map_err(|e| SyncError::Backend(format!("invalid connect request: {e}")))?;

        // Import the ticket and start sync
        let result = do_import_and_sync(&self.client, &self.imported_namespaces, req.ticket).await;

        let resp = match &result {
            Ok(_handle) => ConnectResponse {
                ok: true,
                error: None,
            },
            Err(e) => ConnectResponse {
                ok: false,
                error: Some(e.to_string()),
            },
        };

        // Send response
        let resp_bytes = serde_json::to_vec(&resp)
            .map_err(|e| SyncError::Backend(format!("serialize response: {e}")))?;
        #[allow(clippy::cast_possible_truncation)]
        let resp_len = resp_bytes.len() as u32;
        send.write_u32(resp_len)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        send.write_all(&resp_bytes)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        send.finish()
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        // If import succeeded, spawn a task to drain the sync handle
        if let Ok(mut handle) = result {
            tokio::spawn(async move { while handle.next().await.is_some() {} });
        }

        // Wait for the peer to close the connection (or timeout)
        // so the response data is fully delivered before we drop
        // `conn`.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;

        Ok(())
    }
}

// -- Shared helpers ---------------------------------------------------------

/// Convert an `api::NodeAddr` to an iroh `EndpointAddr`.
pub(crate) fn to_iroh_addr(addr: &api::NodeAddr) -> Result<IrohNodeAddr, SyncError> {
    let node_id = IrohNodeId::from_bytes(addr.node_id.as_bytes())
        .map_err(|e| SyncError::InvalidId(e.to_string()))?;
    let set: BTreeSet<SocketAddr> = addr.direct_addresses.iter().map(|d| d.0).collect();
    let mut iroh_addr = IrohNodeAddr::from_parts(node_id, set.into_iter().map(TransportAddr::Ip));
    if let Some(s) = &addr.relay_url {
        let url: RelayUrl = s
            .as_ref()
            .parse()
            .map_err(|e: iroh::RelayUrlParseError| SyncError::InvalidId(e.to_string()))?;
        iroh_addr = iroh_addr.with_relay_url(url);
    }
    Ok(iroh_addr)
}

/// Convert an `api::NamespaceId` to the iroh-willow type.
pub(crate) fn to_willow_ns(ns: &api::NamespaceId) -> iroh_willow::proto::keys::NamespaceId {
    iroh_willow::proto::keys::NamespaceId::from_bytes_unchecked(*ns.as_bytes())
}

/// Convert an `api::SubspaceId` to a Willow `UserId`.
pub(crate) fn to_willow_user(sub: &api::SubspaceId) -> iroh_willow::proto::keys::UserId {
    iroh_willow::proto::keys::UserId::from_bytes_unchecked(*sub.as_bytes())
}

/// Build an `api::NodeAddr` from an iroh `EndpointAddr`.
pub(crate) fn from_iroh_addr(a: &IrohNodeAddr) -> api::NodeAddr {
    let direct_addresses = a
        .ip_addrs()
        .map(|sa| api::DirectAddress::from(*sa))
        .collect::<Vec<_>>();
    let relay_url = a
        .relay_urls()
        .next()
        .map(|u| api::RelayEndpoint::from(u.to_string()));
    api::NodeAddr {
        node_id: api::NodeId::from_bytes(*a.id.as_bytes()),
        direct_addresses,
        relay_url,
    }
}

// -- Shared connect logic --------------------------------------------------

/// Send a `SyncTicket` to a peer over the mee-connect/0 protocol.
///
/// Extracted from `connect_and_share` so the gossip event loop
/// can reuse the same connection protocol.
pub(crate) async fn send_ticket(
    endpoint: &Endpoint,
    peer_addr: &api::NodeAddr,
    ticket: api::SyncTicket,
) -> Result<(), SyncError> {
    let addr = to_iroh_addr(peer_addr)?;
    let conn = endpoint
        .connect(addr, MEE_CONNECT_ALPN)
        .await
        .map_err(|e| SyncError::Backend(format!("connect to peer: {e}")))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;

    let req = ConnectRequest { ticket };
    let req_bytes = serde_json::to_vec(&req)
        .map_err(|e| SyncError::Backend(format!("serialize request: {e}")))?;
    #[allow(clippy::cast_possible_truncation)]
    let req_len = req_bytes.len() as u32;
    send.write_u32(req_len)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    send.write_all(&req_bytes)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    send.finish()
        .map_err(|e| SyncError::Backend(e.to_string()))?;

    let resp_len = recv
        .read_u32()
        .await
        .map_err(|e| SyncError::Backend(format!("read response length: {e}")))?;
    let mut resp_buf = vec![0u8; resp_len as usize];
    recv.read_exact(&mut resp_buf)
        .await
        .map_err(|e| SyncError::Backend(format!("read response: {e}")))?;

    let resp: ConnectResponse = serde_json::from_slice(&resp_buf)
        .map_err(|e| SyncError::Backend(format!("parse response: {e}")))?;

    conn.close(0u32.into(), b"done");

    if resp.ok {
        Ok(())
    } else {
        Err(SyncError::Backend(
            resp.error
                .unwrap_or_else(|| "peer rejected connection".to_owned()),
        ))
    }
}

// -- Shared import logic ---------------------------------------------------

#[allow(clippy::expect_used)]
async fn do_import_and_sync(
    client: &iroh_willow::rpc::client::MemClient,
    imported_namespaces: &Mutex<HashSet<api::NamespaceId>>,
    ticket: api::SyncTicket,
) -> Result<IrohWillowSyncHandle, SyncError> {
    let caps: Vec<iroh_willow::interest::CapabilityPack> = ticket
        .caps
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let mut nodes: Vec<IrohNodeAddr> = Vec::new();
    for n in &ticket.nodes {
        nodes.push(to_iroh_addr(n)?);
    }
    let space_ticket = iroh_willow::rpc::client::SpaceTicket { caps, nodes };
    let mode = iroh_willow::session::SessionMode::Continuous;
    let (space, mut handles) = client
        .import_and_sync(space_ticket, mode)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    let ns_id = api::NamespaceId::from_bytes(*space.namespace_id().as_bytes());
    imported_namespaces
        .lock()
        .expect("imported_namespaces lock poisoned")
        .insert(ns_id);
    let s = async_stream::stream! {
        use iroh_willow::session::intents::serde_encoding::Event;
        while let Some((_peer, ev)) = handles.next().await {
            let out = match ev {
                Event::CapabilityIntersection { .. } => {
                    api::SyncEvent::CapabilityIntersection
                }
                Event::InterestIntersection { .. } => {
                    api::SyncEvent::InterestIntersection
                }
                Event::Reconciled { .. } => {
                    api::SyncEvent::Reconciled
                }
                Event::ReconciledAll => {
                    api::SyncEvent::ReconciledAll
                }
                Event::Abort { error } => {
                    api::SyncEvent::Abort { error }
                }
            };
            yield out;
        }
    };
    Ok(IrohWillowSyncHandle(Box::pin(s)))
}

// -- Core ------------------------------------------------------------------

pub struct IrohWillowSyncCore {
    endpoint: Endpoint,
    engine: WillowEngine,
    client: iroh_willow::rpc::client::MemClient,
    owner_user: iroh_willow::proto::keys::UserId,
    _router: Router,
    imported_namespaces: Arc<Mutex<HashSet<api::NamespaceId>>>,
    gossip_manager: Option<gossip::GossipManager>,
}

impl IrohWillowSyncCore {
    /// Access the underlying iroh endpoint (for address exchange in tests).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    #[allow(clippy::too_many_lines)]
    pub async fn spawn(config: DiscoveryConfig) -> Result<Self, SyncError> {
        let alpns = vec![ALPN.to_vec(), iroh_gossip::ALPN.to_vec()];
        let mut builder = Endpoint::empty_builder(config.relay_mode).alpns(alpns);

        if config.n0_discovery {
            builder = builder
                .address_lookup(PkarrPublisher::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns());
        }

        #[cfg(feature = "mdns")]
        if config.mdns {
            builder = builder.address_lookup(MdnsAddressLookup::builder());
        }

        if config.clear_ip_transports {
            builder = builder.clear_ip_transports();
        }

        if let Some(addr) = config.bind_addr {
            builder = builder
                .bind_addr(addr)
                .map_err(|e| SyncError::Backend(e.to_string()))?;
        }

        let endpoint = builder
            .bind()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        // TODO(persistent-storage): Switch to persistent storage backends:
        // - iroh_blobs::store::fs::Store (loaded from data_dir)
        // - iroh_willow::store::persistent::Store (redb-backed)
        // Accept data_dir: Option<PathBuf> in spawn(). When None, keep
        // in-memory for tests.
        let blobs = iroh_blobs::store::mem::MemStore::default();
        let create_store = move || iroh_willow::store::memory::Store::new(blobs.clone());
        let engine = WillowEngine::spawn(endpoint.clone(), create_store, AcceptOpts::default());

        let client: iroh_willow::rpc::client::MemClient = engine.client().clone();
        let owner_user = client
            .create_user()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        let imported_namespaces = Arc::new(Mutex::new(HashSet::new()));

        let connect_handler = ConnectHandler {
            client: client.clone(),
            imported_namespaces: imported_namespaces.clone(),
        };

        // Create Gossip instance before Router
        let gossip_instance = iroh_gossip::Gossip::builder().spawn(endpoint.clone());

        let mut router_builder = Router::builder(endpoint.clone())
            .accept(ALPN, engine.clone())
            .accept(MEE_CONNECT_ALPN, connect_handler);

        router_builder = router_builder.accept(iroh_gossip::ALPN, gossip_instance.clone());

        let router = router_builder.spawn();

        // Start gossip manager after Router is up
        let gossip_manager = if let Some(gossip_config) = config.gossip {
            Some(
                gossip::GossipManager::start(
                    gossip_instance,
                    endpoint.clone(),
                    engine.clone(),
                    client.clone(),
                    imported_namespaces.clone(),
                    owner_user,
                    gossip_config,
                )
                .await
                .map_err(|e| SyncError::Backend(e.to_string()))?,
            )
        } else {
            None
        };

        Ok(Self {
            endpoint,
            engine,
            client,
            owner_user,
            _router: router,
            imported_namespaces,
            gossip_manager,
        })
    }

    /// Access the gossip manager (if gossip is enabled).
    pub fn gossip_manager(&self) -> Option<&gossip::GossipManager> {
        self.gossip_manager.as_ref()
    }
}

pub struct IrohWillowSyncHandle(BoxStream<'static, api::SyncEvent>);

impl Stream for IrohWillowSyncHandle {
    type Item = api::SyncEvent;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.0).poll_next(cx)
    }
}

#[allow(async_fn_in_trait)]
impl api::SyncHandle for IrohWillowSyncHandle {
    fn complete<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn futures_core::Future<Output = Result<(), SyncError>> + Send + 'a>> {
        Box::pin(async move {
            while let Some(_ev) = self.next().await {}
            Ok(())
        })
    }
}

#[allow(async_fn_in_trait, clippy::expect_used, clippy::as_conversions)]
impl api::SyncEngine for IrohWillowSyncCore {
    async fn addr(&self) -> Result<api::NodeAddr, SyncError> {
        let a = self
            .client
            .node_addr()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(from_iroh_addr(&a))
    }

    async fn subspace_id(&self) -> Result<api::SubspaceId, SyncError> {
        Ok(api::SubspaceId::from_bytes(*self.owner_user.as_bytes()))
    }

    #[allow(clippy::expect_used)]
    async fn create_namespace(
        &self,
        owner: &api::SubspaceId,
    ) -> Result<api::NamespaceId, SyncError> {
        let u = to_willow_user(owner);
        let space = self
            .client
            .create(iroh_willow::proto::keys::NamespaceKind::Owned, u)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let ns_id = api::NamespaceId::from_bytes(*space.namespace_id().as_bytes());
        // Track for gossip advertisement
        self.imported_namespaces
            .lock()
            .expect("imported_namespaces lock poisoned")
            .insert(ns_id);
        Ok(ns_id)
    }

    async fn list_namespaces(&self) -> Result<Vec<api::NamespaceId>, SyncError> {
        let v = self
            .engine
            .list_namespaces()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        // include namespaces we imported via tickets
        let imported = self
            .imported_namespaces
            .lock()
            .expect("imported_namespaces lock poisoned")
            .clone();
        let mut out: Vec<api::NamespaceId> = v
            .into_iter()
            .map(|ns| api::NamespaceId::from_bytes(*ns.as_bytes()))
            .collect();
        out.extend(imported);
        Ok(out)
    }

    async fn share(
        &self,
        ns: &api::NamespaceId,
        to: &api::SubspaceId,
        access: api::AccessMode,
    ) -> Result<api::SyncTicket, SyncError> {
        use iroh_willow::interest::{CapSelector, DelegateTo, RestrictArea};
        use iroh_willow::proto::meadowcap::AccessMode;
        let willow_ns = to_willow_ns(ns);
        let willow_user = to_willow_user(to);
        let access = match access {
            api::AccessMode::Read => AccessMode::Read,
            api::AccessMode::Write => AccessMode::Write,
        };
        let caps = self
            .client
            .delegate_caps(
                CapSelector::any(willow_ns),
                access,
                DelegateTo::new(willow_user, RestrictArea::None),
            )
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let mut addr = self
            .client
            .node_addr()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let first_port = addr.ip_addrs().next().map(SocketAddr::port);
        if let Some(port) = first_port {
            let loopback: SocketAddr = format!("127.0.0.1:{port}").parse()?;
            addr.addrs.insert(TransportAddr::Ip(loopback));
        }
        let node_addr = from_iroh_addr(&addr);
        Ok(api::SyncTicket {
            caps: caps
                .into_iter()
                // TODO: Propagate serialization errors instead of silently falling
                // back to empty JSON object.
                .map(|c| serde_json::to_value(&c).unwrap_or(serde_json::json!({})))
                .collect(),
            nodes: vec![node_addr],
        })
    }

    // TODO: Respect SyncMode parameter. Currently always uses Continuous
    // mode regardless of caller request (ReconcileOnce is ignored).
    async fn import_and_sync(
        &self,
        ticket: api::SyncTicket,
        _mode: api::SyncMode,
    ) -> Result<Box<dyn api::SyncHandle>, SyncError> {
        let handle = do_import_and_sync(&self.client, &self.imported_namespaces, ticket).await?;
        Ok(Box::new(handle))
    }

    async fn connect_and_share(
        &self,
        ns: &api::NamespaceId,
        to: &api::SubspaceId,
        peer_addr: &api::NodeAddr,
        access: api::AccessMode,
    ) -> Result<(), SyncError> {
        let ticket = self.share(ns, to, access).await?;
        send_ticket(&self.endpoint, peer_addr, ticket).await
    }

    async fn insert(
        &self,
        ns: &api::NamespaceId,
        path: &api::EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError> {
        let willow_ns = to_willow_ns(ns);
        let comps: Vec<Vec<u8>> = path
            .as_str()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let comp_refs: Vec<&[u8]> = comps.iter().map(std::vec::Vec::as_slice).collect();
        let willow_path = iroh_willow::proto::data_model::Path::from_bytes(&comp_refs)
            .map_err(|e| SyncError::InvalidNamespace(format!("invalid path: {e:?}")))?;
        let entry_form =
            iroh_willow::form::EntryForm::new_bytes(willow_ns, willow_path, bytes.to_vec());
        self.engine
            .insert_entry(
                entry_form,
                iroh_willow::form::AuthForm::Any(self.owner_user),
            )
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(())
    }

    type EntryStream = BoxStream<'static, Result<api::EntryInfo, SyncError>>;

    async fn get_entries(&self, ns: &api::NamespaceId) -> Result<Self::EntryStream, SyncError> {
        let willow_ns = to_willow_ns(ns);
        let range = iroh_willow::proto::grouping::Range3d::new_full();
        let mut stream = self
            .engine
            .get_entries(willow_ns, range)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let s = async_stream::try_stream! {
            while let Some(item) = stream.next().await {
                let e = item
                    .map_err(|e| {
                        SyncError::Backend(e.to_string())
                    })?;
                let entry = e.entry();
                let path_str = entry.path().fmt_utf8();
                let path = api::EntryPath::new(path_str)
                    .map_err(|e| {
                        SyncError::Backend(format!(
                            "invalid entry path: {e}"
                        ))
                    })?;
                yield api::EntryInfo {
                    namespace: api::NamespaceId::from_bytes(
                        *entry.namespace_id().as_bytes(),
                    ),
                    subspace: api::SubspaceId::from_bytes(
                        *entry.subspace_id().as_bytes(),
                    ),
                    path,
                    payload_len: entry.payload_length()
                };
            }
        };
        Ok(Box::pin(s))
    }
}
