pub mod gossip;

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
use std::{collections::BTreeSet, net::SocketAddr, pin::Pin};
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
    /// Default config: gossip on, no relay, no mDNS.
    ///
    /// Relay and mDNS can be toggled via env vars (`MEE_RELAY=1`,
    /// `MEE_MDNS=1`) when needed for internet deployment.
    pub fn default_config() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            mdns: false,
            n0_discovery: false,
            bind_addr: None,
            clear_ip_transports: false,
            gossip: Some(gossip::GossipConfig::default_config()),
        }
    }

    /// Enable relay + DNS/pkarr discovery (n0 infrastructure).
    pub fn enable_relay(&mut self) {
        self.relay_mode = RelayMode::Default;
        self.n0_discovery = true;
    }

    /// Enable mDNS for local network peer discovery.
    pub fn enable_mdns(&mut self) {
        self.mdns = true;
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
    engine: WillowEngine,
    owner_user: iroh_willow::proto::keys::UserId,
    home_namespace: api::NamespaceId,
    gossip_cmd_tx: Option<tokio::sync::mpsc::Sender<gossip::GossipCommand>>,
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
        let result = do_import_and_sync(
            &self.client,
            &self.engine,
            self.owner_user,
            &self.home_namespace,
            req.ticket,
        )
        .await;

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
        // and auto-join the connecting peer in gossip.
        if let Ok(mut handle) = result {
            if let Some(ref tx) = self.gossip_cmd_tx {
                let peer_eid = conn.remote_id();
                let _ = tx
                    .send(gossip::GossipCommand::JoinPeers(vec![peer_eid]))
                    .await;
            }
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

/// Add a loopback address to an iroh `EndpointAddr` if it has a known port.
///
/// Used so that local connections (same-machine, Docker bridge) can
/// reach the peer even when the OS doesn't report localhost as a
/// transport address.
pub(crate) fn add_loopback_addr(addr: &mut IrohNodeAddr) {
    let first_port = addr.ip_addrs().next().map(SocketAddr::port);
    if let Some(port) = first_port {
        let loopback = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
        );
        addr.addrs.insert(TransportAddr::Ip(loopback));
    }
}

// -- Willow _local/ path helpers -------------------------------------------

/// Prefix for all node-local state stored in Willow.
/// Entries under `_local/` are not synced to peers.
const LOCAL_PREFIX: &str = "_local/";

/// Build an `EntryPath` under `_local/`.
fn local_path(suffix: &str) -> Result<api::EntryPath, SyncError> {
    api::EntryPath::new(format!("{LOCAL_PREFIX}{suffix}"))
        .map_err(|e| SyncError::Backend(format!("invalid local path: {e}")))
}

/// Write a JSON value to a `_local/` path in the given namespace.
pub(crate) async fn write_local_json<T: serde::Serialize>(
    engine: &WillowEngine,
    owner: iroh_willow::proto::keys::UserId,
    ns: &api::NamespaceId,
    suffix: &str,
    value: &T,
) -> Result<(), SyncError> {
    let path = local_path(suffix)?;
    let bytes =
        serde_json::to_vec(value).map_err(|e| SyncError::Backend(format!("serialize: {e}")))?;
    insert_raw(engine, owner, ns, &path, &bytes).await
}

/// Read a JSON value from a `_local/` path in the given namespace.
#[allow(dead_code)]
pub(crate) async fn read_local_json<T: serde::de::DeserializeOwned>(
    engine: &WillowEngine,
    blobs: &iroh_blobs::api::Store,
    ns: &api::NamespaceId,
    suffix: &str,
) -> Result<Option<T>, SyncError> {
    let path = local_path(suffix)?;
    let bytes = read_entry_payload_raw(engine, blobs, ns, &path).await?;
    match bytes {
        Some(b) if !b.is_empty() => {
            let val = serde_json::from_slice(&b)
                .map_err(|e| SyncError::Backend(format!("deserialize: {e}")))?;
            Ok(Some(val))
        }
        _ => Ok(None),
    }
}

/// Write raw bytes to a `_local/` path.
pub(crate) async fn write_local_raw(
    engine: &WillowEngine,
    owner: iroh_willow::proto::keys::UserId,
    ns: &api::NamespaceId,
    suffix: &str,
    bytes: &[u8],
) -> Result<(), SyncError> {
    let path = local_path(suffix)?;
    insert_raw(engine, owner, ns, &path, bytes).await
}

/// List all `_local/` entries whose suffix starts with the given
/// prefix. Returns `(suffix, payload_bytes)` pairs.
pub(crate) async fn list_local_entries(
    engine: &WillowEngine,
    blobs: &iroh_blobs::api::Store,
    ns: &api::NamespaceId,
    suffix_prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>, SyncError> {
    let full_prefix = format!("{LOCAL_PREFIX}{suffix_prefix}");
    let willow_ns = to_willow_ns(ns);
    let range = iroh_willow::proto::grouping::Range3d::new_full();
    let mut stream = engine
        .get_entries(willow_ns, range)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;

    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let entry = item.map_err(|e| SyncError::Backend(e.to_string()))?;
        let entry_path = entry.entry().path().fmt_utf8();
        if let Some(suffix) = entry_path.strip_prefix(&full_prefix) {
            let hash = entry.entry().payload_digest().0;
            let bytes = blobs
                .blobs()
                .get_bytes(hash)
                .await
                .map_err(|e| SyncError::Backend(format!("blob read: {e}")))?;
            out.push((suffix.to_owned(), bytes.to_vec()));
        }
    }
    Ok(out)
}

/// Delete a `_local/` entry by writing an empty payload (tombstone).
pub(crate) async fn delete_local(
    engine: &WillowEngine,
    owner: iroh_willow::proto::keys::UserId,
    ns: &api::NamespaceId,
    suffix: &str,
) -> Result<(), SyncError> {
    write_local_raw(engine, owner, ns, suffix, &[]).await
}

// -- Low-level insert/read -------------------------------------------------

/// Insert raw bytes at an arbitrary `EntryPath`.
async fn insert_raw(
    engine: &WillowEngine,
    owner: iroh_willow::proto::keys::UserId,
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
    engine
        .insert_entry(entry_form, iroh_willow::form::AuthForm::Any(owner))
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    Ok(())
}

/// Read the payload bytes at an arbitrary `EntryPath`.
async fn read_entry_payload_raw(
    engine: &WillowEngine,
    blobs: &iroh_blobs::api::Store,
    ns: &api::NamespaceId,
    path: &api::EntryPath,
) -> Result<Option<Vec<u8>>, SyncError> {
    let willow_ns = to_willow_ns(ns);
    let range = iroh_willow::proto::grouping::Range3d::new_full();
    let mut stream = engine
        .get_entries(willow_ns, range)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;

    while let Some(item) = stream.next().await {
        let entry = item.map_err(|e| SyncError::Backend(e.to_string()))?;
        let entry_path = entry.entry().path().fmt_utf8();
        if entry_path == path.as_str() {
            let hash = entry.entry().payload_digest().0;
            let bytes = blobs
                .blobs()
                .get_bytes(hash)
                .await
                .map_err(|e| SyncError::Backend(format!("blob read: {e}")))?;
            return Ok(Some(bytes.to_vec()));
        }
    }
    Ok(None)
}

// -- Shared connect logic --------------------------------------------------

/// Send a `SyncTicket` to a peer over the mee-connect/0 protocol.
///
/// Extracted from `connect` so the gossip event loop
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

async fn do_import_and_sync(
    client: &iroh_willow::rpc::client::MemClient,
    engine: &WillowEngine,
    owner_user: iroh_willow::proto::keys::UserId,
    home_namespace: &api::NamespaceId,
    ticket: api::SyncTicket,
) -> Result<IrohWillowSyncHandle, SyncError> {
    let mut caps: Vec<iroh_willow::interest::CapabilityPack> = Vec::new();
    for (i, v) in ticket.caps.into_iter().enumerate() {
        let cap = serde_json::from_value(v).map_err(|e| {
            SyncError::Backend(format!("capability {i} failed to deserialize: {e}"))
        })?;
        caps.push(cap);
    }
    let mut nodes: Vec<IrohNodeAddr> = Vec::new();
    for n in &ticket.node_hints {
        nodes.push(to_iroh_addr(n)?);
    }
    let space_ticket = iroh_willow::rpc::client::SpaceTicket { caps, nodes };
    let mode = iroh_willow::session::SessionMode::Continuous;
    let (space, mut handles) = client
        .import_and_sync(space_ticket, mode)
        .await
        .map_err(|e| SyncError::Backend(e.to_string()))?;
    let ns_id = api::NamespaceId::from_bytes(*space.namespace_id().as_bytes());
    // Track imported namespace in Willow _local/ path
    let suffix = format!("namespaces/imported/{ns_id}");
    let _ = write_local_raw(engine, owner_user, home_namespace, &suffix, &[]).await;
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
    blobs: iroh_blobs::api::Store,
    home_namespace: api::NamespaceId,
    _router: Router,
    gossip_manager: Option<gossip::GossipManager>,
}

impl IrohWillowSyncCore {
    /// Access the underlying iroh endpoint (for address exchange in tests).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The node's home namespace, created at spawn time.
    pub fn home_namespace(&self) -> api::NamespaceId {
        self.home_namespace
    }

    /// Access the Willow engine (for direct entry operations).
    pub fn engine(&self) -> &WillowEngine {
        &self.engine
    }

    /// Access the blob store (for payload reads).
    pub fn blobs(&self) -> &iroh_blobs::api::Store {
        &self.blobs
    }

    /// The owner user (subspace) ID.
    pub fn owner_user(&self) -> iroh_willow::proto::keys::UserId {
        self.owner_user
    }

    // -- Public local-state helpers (home namespace) --

    /// Write a JSON value to `_local/{suffix}` in the home namespace.
    pub async fn put_local_json<T: serde::Serialize>(
        &self,
        suffix: &str,
        value: &T,
    ) -> Result<(), SyncError> {
        write_local_json(
            &self.engine,
            self.owner_user,
            &self.home_namespace,
            suffix,
            value,
        )
        .await
    }

    /// Read a JSON value from `_local/{suffix}` in the home namespace.
    pub async fn get_local_json<T: serde::de::DeserializeOwned>(
        &self,
        suffix: &str,
    ) -> Result<Option<T>, SyncError> {
        read_local_json(&self.engine, &self.blobs, &self.home_namespace, suffix).await
    }

    /// List `_local/` entries whose suffix starts with the given prefix.
    /// Returns `(suffix, payload_bytes)` pairs.
    pub async fn list_local(
        &self,
        suffix_prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, SyncError> {
        list_local_entries(
            &self.engine,
            &self.blobs,
            &self.home_namespace,
            suffix_prefix,
        )
        .await
    }

    /// Delete a `_local/` entry (tombstone) in the home namespace.
    pub async fn remove_local(&self, suffix: &str) -> Result<(), SyncError> {
        delete_local(
            &self.engine,
            self.owner_user,
            &self.home_namespace,
            suffix,
        )
        .await
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
        let blobs_api: iroh_blobs::api::Store = (*blobs).clone();
        let create_store = move || iroh_willow::store::memory::Store::new(blobs.clone());
        let engine = WillowEngine::spawn(endpoint.clone(), create_store, AcceptOpts::default());

        let client: iroh_willow::rpc::client::MemClient = engine.client().clone();
        let owner_user = client
            .create_user()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        // Create the home namespace
        let space = client
            .create(iroh_willow::proto::keys::NamespaceKind::Owned, owner_user)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let home_namespace = api::NamespaceId::from_bytes(*space.namespace_id().as_bytes());

        // Track home namespace as an imported namespace marker so gossip
        // advertises it.
        let suffix = format!("namespaces/imported/{home_namespace}");
        let _ = write_local_raw(&engine, owner_user, &home_namespace, &suffix, &[]).await;

        // Create gossip command channel early so ConnectHandler can
        // auto-join peers before GossipManager is fully started.
        let gossip_cmd_tx = if config.gossip.is_some() {
            Some(tokio::sync::mpsc::channel::<gossip::GossipCommand>(16))
        } else {
            None
        };

        let connect_handler = ConnectHandler {
            client: client.clone(),
            engine: engine.clone(),
            owner_user,
            home_namespace,
            gossip_cmd_tx: gossip_cmd_tx.as_ref().map(|(tx, _)| tx.clone()),
        };

        // Create Gossip instance before Router
        let gossip_instance = iroh_gossip::Gossip::builder().spawn(endpoint.clone());

        let mut router_builder = Router::builder(endpoint.clone())
            .accept(ALPN, engine.clone())
            .accept(MEE_CONNECT_ALPN, connect_handler);

        router_builder = router_builder.accept(iroh_gossip::ALPN, gossip_instance.clone());

        let router = router_builder.spawn();

        // Start gossip manager after Router is up, passing the
        // pre-created command channel.
        let gossip_manager = if let Some(gossip_config) = config.gossip {
            let (cmd_tx, cmd_rx) = gossip_cmd_tx.expect("channel created when gossip enabled");
            Some(
                gossip::GossipManager::start_with_channel(
                    gossip_instance,
                    endpoint.clone(),
                    engine.clone(),
                    client.clone(),
                    owner_user,
                    gossip_config,
                    cmd_tx,
                    cmd_rx,
                    blobs_api.clone(),
                    home_namespace,
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
            blobs: blobs_api,
            home_namespace,
            _router: router,
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
        // Track for gossip advertisement via _local/ path
        let suffix = format!("namespaces/imported/{ns_id}");
        let _ = write_local_raw(
            &self.engine,
            self.owner_user,
            &self.home_namespace,
            &suffix,
            &[],
        )
        .await;
        Ok(ns_id)
    }

    async fn list_namespaces(&self) -> Result<Vec<api::NamespaceId>, SyncError> {
        let v = self
            .engine
            .list_namespaces()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<api::NamespaceId> = Vec::new();
        for ns in v {
            let id = api::NamespaceId::from_bytes(*ns.as_bytes());
            if seen.insert(id) {
                out.push(id);
            }
        }
        // include namespaces we imported via tickets (tracked in _local/)
        if let Ok(entries) = list_local_entries(
            &self.engine,
            &self.blobs,
            &self.home_namespace,
            "namespaces/imported/",
        )
        .await
        {
            for (suffix, _) in entries {
                if let Ok(id) = suffix.parse::<api::NamespaceId>() {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
        }
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
        add_loopback_addr(&mut addr);
        let node_addr = from_iroh_addr(&addr);
        let mut cap_values = Vec::with_capacity(caps.len());
        for (i, c) in caps.into_iter().enumerate() {
            cap_values.push(serde_json::to_value(&c).map_err(|e| {
                SyncError::Backend(format!("capability {i} serialization failed: {e}"))
            })?);
        }
        Ok(api::SyncTicket {
            caps: cap_values,
            node_hints: vec![node_addr],
        })
    }

    // TODO: Respect SyncMode parameter. Currently always uses Continuous
    // mode regardless of caller request (ReconcileOnce is ignored).
    async fn import_and_sync(
        &self,
        ticket: api::SyncTicket,
        _mode: api::SyncMode,
    ) -> Result<Box<dyn api::SyncHandle>, SyncError> {
        let handle = do_import_and_sync(
            &self.client,
            &self.engine,
            self.owner_user,
            &self.home_namespace,
            ticket,
        )
        .await?;
        Ok(Box::new(handle))
    }

    async fn connect(
        &self,
        ns: &api::NamespaceId,
        to: &api::SubspaceId,
        peer_addr: &api::NodeAddr,
        access: api::AccessMode,
    ) -> Result<(), SyncError> {
        let ticket = self.share(ns, to, access).await?;
        send_ticket(&self.endpoint, peer_addr, ticket).await?;

        // Auto-join peer in gossip mesh
        if let Some(gm) = self.gossip_manager() {
            if let Ok(eid) = iroh::EndpointId::from_bytes(peer_addr.node_id.as_bytes()) {
                let _ = gm.join_peers(vec![eid]).await;
            }
        }

        Ok(())
    }

    async fn insert(
        &self,
        ns: &api::NamespaceId,
        path: &api::EntryPath,
        bytes: &[u8],
    ) -> Result<(), SyncError> {
        insert_raw(&self.engine, self.owner_user, ns, path, bytes).await
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

    async fn read_entry_payload(
        &self,
        ns: &api::NamespaceId,
        path: &api::EntryPath,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        read_entry_payload_raw(&self.engine, &self.blobs, ns, path).await
    }
}
