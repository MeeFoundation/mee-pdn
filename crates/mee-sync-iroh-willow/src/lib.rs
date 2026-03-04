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
    str::FromStr,
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
}

impl DiscoveryConfig {
    /// No discovery, no relay. Current localhost-only behavior.
    pub fn disabled() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: false,
            n0_discovery: false,
            bind_addr: None,
        }
    }

    /// mDNS only — discovers peers on the local network, no relay.
    pub fn local() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: true,
            n0_discovery: false,
            bind_addr: None,
        }
    }

    /// Full discovery: relay + DNS/pkarr + mDNS.
    pub fn full() -> Self {
        Self {
            relay_mode: RelayMode::Default,
            mdns: true,
            n0_discovery: true,
            bind_addr: None,
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
    imported_namespaces: Arc<Mutex<HashSet<String>>>,
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
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
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

        // Wait for the peer to close the connection (or timeout) so the
        // response data is fully delivered before we drop `conn`.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;

        Ok(())
    }
}

// -- Shared import logic ---------------------------------------------------

#[allow(clippy::expect_used)]
async fn do_import_and_sync(
    client: &iroh_willow::rpc::client::MemClient,
    imported_namespaces: &Mutex<HashSet<String>>,
    ticket: api::SyncTicket,
) -> Result<IrohWillowSyncHandle, SyncError> {
    let caps: Vec<iroh_willow::interest::CapabilityPack> = ticket
        .caps
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let mut nodes: Vec<IrohNodeAddr> = Vec::new();
    for n in &ticket.nodes {
        let node_id = IrohNodeId::from_str(n.node_id.as_ref())
            .map_err(|e| SyncError::InvalidId(e.to_string()))?;
        let set: BTreeSet<SocketAddr> = n
            .direct_addresses
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let mut addr = IrohNodeAddr::from_parts(node_id, set.into_iter().map(TransportAddr::Ip));
        if let Some(s) = &n.relay_url {
            let url =
                RelayUrl::from_str(s.as_ref()).map_err(|e| SyncError::InvalidId(e.to_string()))?;
            addr = addr.with_relay_url(url);
        }
        nodes.push(addr);
    }
    let space_ticket = iroh_willow::rpc::client::SpaceTicket { caps, nodes };
    let mode = iroh_willow::session::SessionMode::Continuous;
    let (space, mut handles) = client
        .import_and_sync(space_ticket, mode)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    let ns = format!("{}", space.namespace_id());
    imported_namespaces
        .lock()
        .expect("imported_namespaces lock poisoned")
        .insert(ns);
    let s = async_stream::stream! {
        use iroh_willow::session::intents::serde_encoding::Event;
        while let Some((_peer, ev)) = handles.next().await {
            let out = match ev {
                Event::CapabilityIntersection { .. } => api::SyncEvent::CapabilityIntersection,
                Event::InterestIntersection { .. } => api::SyncEvent::InterestIntersection,
                Event::Reconciled { .. } => api::SyncEvent::Reconciled,
                Event::ReconciledAll => api::SyncEvent::ReconciledAll,
                Event::Abort { error } => api::SyncEvent::Abort { error },
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
    imported_namespaces: Arc<Mutex<HashSet<String>>>,
}

impl IrohWillowSyncCore {
    pub async fn spawn(config: DiscoveryConfig) -> Result<Self, SyncError> {
        let mut builder = Endpoint::empty_builder(config.relay_mode).alpns(vec![ALPN.to_vec()]);

        if config.n0_discovery {
            builder = builder
                .address_lookup(PkarrPublisher::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns());
        }

        #[cfg(feature = "mdns")]
        if config.mdns {
            builder = builder.address_lookup(MdnsAddressLookup::builder());
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

        let router = Router::builder(endpoint.clone())
            .accept(ALPN, engine.clone())
            .accept(MEE_CONNECT_ALPN, connect_handler)
            .spawn();

        Ok(Self {
            endpoint,
            engine,
            client,
            owner_user,
            _router: router,
            imported_namespaces,
        })
    }
}

impl IrohWillowSyncCore {
    /// Concrete import-and-sync returning the typed handle (used by the session manager).
    pub(crate) async fn import_and_sync_inner(
        &self,
        ticket: api::SyncTicket,
    ) -> Result<IrohWillowSyncHandle, SyncError> {
        do_import_and_sync(&self.client, &self.imported_namespaces, ticket).await
    }
}

// Split-responsibility manager façade moved into managers module.
pub mod managers;

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
        let direct_addresses = a
            .ip_addrs()
            .map(|sa| api::DirectAddress::from(*sa))
            .collect::<Vec<_>>();
        let relay_url = a
            .relay_urls()
            .next()
            .map(|u| api::RelayEndpoint::from(u.to_string()));
        Ok(api::NodeAddr {
            node_id: api::NodeId::from(format!("{}", a.id)),
            direct_addresses,
            relay_url,
        })
    }

    async fn user_id(&self) -> Result<api::TransportUserId, SyncError> {
        Ok(api::TransportUserId(
            data_encoding::BASE32_NOPAD.encode(self.owner_user.as_bytes()),
        ))
    }

    async fn create_namespace(
        &self,
        owner: &api::TransportUserId,
    ) -> Result<api::NamespaceId, SyncError> {
        let u = owner
            .0
            .parse::<iroh_willow::proto::keys::UserId>()
            .map_err(|e| SyncError::InvalidId(e.to_string()))?;
        let space = self
            .client
            .create(iroh_willow::proto::keys::NamespaceKind::Owned, u)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(api::NamespaceId(format!("{}", space.namespace_id())))
    }

    async fn list_namespaces(&self) -> Result<Vec<api::NamespaceId>, SyncError> {
        let v = self
            .engine
            .list_namespaces()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        // include namespaces we imported via tickets (provider namespaces)
        let imported = self
            .imported_namespaces
            .lock()
            .expect("imported_namespaces lock poisoned")
            .clone();
        let mut out: Vec<api::NamespaceId> = v
            .into_iter()
            .map(|ns| api::NamespaceId(format!("{ns}")))
            .collect();
        for s in imported {
            out.push(api::NamespaceId(s));
        }
        Ok(out)
    }

    async fn share(
        &self,
        ns: &api::NamespaceId,
        to: &api::TransportUserId,
        access: api::AccessMode,
    ) -> Result<api::SyncTicket, SyncError> {
        use iroh_willow::interest::{CapSelector, DelegateTo, RestrictArea};
        use iroh_willow::proto::meadowcap::AccessMode;
        let ns =
            ns.0.parse::<iroh_willow::proto::keys::NamespaceId>()
                .or_else(|_| {
                    let bytes = hex::decode(&ns.0)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    let arr: [u8; 32] = bytes
                        .try_into()
                        .map_err(|_| SyncError::InvalidNamespace("invalid ns length".to_owned()))?;
                    let pk = iroh_willow::proto::keys::NamespacePublicKey::from_bytes(&arr)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    Ok::<_, SyncError>(pk.id())
                })?;
        let to =
            to.0.parse::<iroh_willow::proto::keys::UserId>()
                .map_err(|e| SyncError::InvalidId(e.to_string()))?;
        let access = match access {
            api::AccessMode::Read => AccessMode::Read,
            api::AccessMode::Write => AccessMode::Write,
        };
        let caps = self
            .client
            .delegate_caps(
                CapSelector::any(ns),
                access,
                DelegateTo::new(to, RestrictArea::None),
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
        let direct_addresses = addr
            .ip_addrs()
            .map(|sa| api::DirectAddress::from(*sa))
            .collect::<Vec<_>>();
        let relay_url = addr
            .relay_urls()
            .next()
            .map(|u| api::RelayEndpoint::from(u.to_string()));
        Ok(api::SyncTicket {
            caps: caps
                .into_iter()
                .map(|c| serde_json::to_value(&c).unwrap_or(serde_json::json!({})))
                .collect(),
            nodes: vec![api::NodeAddr {
                node_id: api::NodeId::from(format!("{}", addr.id)),
                direct_addresses,
                relay_url,
            }],
        })
    }

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
        to: &api::TransportUserId,
        peer_addr: &api::NodeAddr,
        access: api::AccessMode,
    ) -> Result<(), SyncError> {
        // 1. Delegate capabilities and build a ticket
        let ticket = self.share(ns, to, access).await?;

        // 2. Resolve peer's iroh address
        let node_id = IrohNodeId::from_str(peer_addr.node_id.as_ref())
            .map_err(|e| SyncError::InvalidId(e.to_string()))?;
        let set: BTreeSet<SocketAddr> = peer_addr
            .direct_addresses
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let mut addr = IrohNodeAddr::from_parts(node_id, set.into_iter().map(TransportAddr::Ip));
        if let Some(s) = &peer_addr.relay_url {
            let url =
                RelayUrl::from_str(s.as_ref()).map_err(|e| SyncError::InvalidId(e.to_string()))?;
            addr = addr.with_relay_url(url);
        }

        // 3. Connect to peer over mee-connect ALPN
        let conn = self
            .endpoint
            .connect(addr, MEE_CONNECT_ALPN)
            .await
            .map_err(|e| SyncError::Backend(format!("connect to peer: {e}")))?;

        // 4. Send ticket over bi-stream
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        let req = ConnectRequest { ticket };
        let req_bytes = serde_json::to_vec(&req)
            .map_err(|e| SyncError::Backend(format!("serialize request: {e}")))?;
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let req_len = req_bytes.len() as u32;
        send.write_u32(req_len)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        send.write_all(&req_bytes)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        send.finish()
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        // 5. Read response
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

        // Close the connection gracefully so the peer can stop waiting.
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

    async fn insert(
        &self,
        ns: &api::NamespaceId,
        path: &api::EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError> {
        let ns =
            ns.0.parse::<iroh_willow::proto::keys::NamespaceId>()
                .or_else(|_| {
                    let b = hex::decode(&ns.0)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    let arr: [u8; 32] = b
                        .try_into()
                        .map_err(|_| SyncError::InvalidNamespace("invalid ns length".to_owned()))?;
                    let pk = iroh_willow::proto::keys::NamespacePublicKey::from_bytes(&arr)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    Ok::<_, SyncError>(pk.id())
                })?;
        let comps: Vec<Vec<u8>> = path
            .as_ref()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let comp_refs: Vec<&[u8]> = comps.iter().map(std::vec::Vec::as_slice).collect();
        let path = iroh_willow::proto::data_model::Path::from_bytes(&comp_refs)
            .map_err(|e| SyncError::InvalidNamespace(format!("invalid path: {e:?}")))?;
        let entry_form = iroh_willow::form::EntryForm::new_bytes(ns, path, bytes.to_vec());
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
        let ns =
            ns.0.parse::<iroh_willow::proto::keys::NamespaceId>()
                .or_else(|_| {
                    let b = hex::decode(&ns.0)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    let arr: [u8; 32] = b
                        .try_into()
                        .map_err(|_| SyncError::InvalidNamespace("invalid ns length".to_owned()))?;
                    let pk = iroh_willow::proto::keys::NamespacePublicKey::from_bytes(&arr)
                        .map_err(|e| SyncError::InvalidNamespace(e.to_string()))?;
                    Ok::<_, SyncError>(pk.id())
                })?;
        let range = iroh_willow::proto::grouping::Range3d::new_full();
        let mut stream = self
            .engine
            .get_entries(ns, range)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let s = async_stream::try_stream! {
            while let Some(item) = stream.next().await {
                let e = item.map_err(|e| SyncError::Backend(e.to_string()))?;
                let entry = e.entry();
                yield api::EntryInfo {
                    namespace: api::NamespaceId(format!("{}", entry.namespace_id())),
                    subspace_hex: api::SubspaceId::from(entry.subspace_id().to_string()),
                    path: api::EntryPath::from(entry.path().fmt_utf8()),
                    payload_len: entry.payload_length()
                };
            }
        };
        Ok(Box::pin(s))
    }
}
