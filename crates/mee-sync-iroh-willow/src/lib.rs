use futures_core::Stream;
use futures_util::stream::{BoxStream, StreamExt as _};
use iroh::{Endpoint, NodeAddr as IrohNodeAddr, NodeId as IrohNodeId, RelayUrl};
use iroh_willow::proto::data_model::PathExt;
use iroh_willow::{engine::AcceptOpts, Engine as WillowEngine, ALPN};
use mee_sync_api as api;
use mee_sync_api::SyncError;
use std::{
    collections::{BTreeSet, HashSet},
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::{Arc, Mutex},
};

pub struct IrohWillowSyncCore {
    _endpoint: Endpoint,
    engine: WillowEngine,
    client: iroh_willow::rpc::client::MemClient,
    owner_user: iroh_willow::proto::keys::UserId,
    _accept_task: tokio::task::JoinHandle<()>,
    imported_namespaces: Arc<Mutex<HashSet<String>>>,
}

impl IrohWillowSyncCore {
    pub async fn spawn() -> Result<Self, SyncError> {
        let endpoint = Endpoint::builder()
            .relay_mode(iroh::RelayMode::Disabled)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        let blobs = iroh_blobs::store::mem::MemStore::default();
        let create_store = move || iroh_willow::store::memory::Store::new(blobs.clone());
        let engine = WillowEngine::spawn(endpoint.clone(), create_store, AcceptOpts::default());

        let accept_task = tokio::spawn({
            let engine = engine.clone();
            let endpoint = endpoint.clone();
            async move {
                while let Some(incoming) = endpoint.accept().await {
                    let Ok(mut connecting) = incoming.accept() else {
                        continue;
                    };
                    let Ok(alpn) = connecting.alpn().await else {
                        continue;
                    };
                    if alpn != ALPN {
                        continue;
                    }
                    let Ok(conn) = connecting.await else {
                        continue;
                    };
                    let _ = engine.handle_connection(conn).await;
                }
            }
        });

        let client: iroh_willow::rpc::client::MemClient = engine.client().clone();
        let owner_user = client
            .create_user()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;

        Ok(Self {
            _endpoint: endpoint,
            engine,
            client,
            owner_user,
            _accept_task: accept_task,
            imported_namespaces: Arc::new(Mutex::new(HashSet::new())),
        })
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

#[allow(async_fn_in_trait, clippy::expect_used)]
impl api::SyncEngine for IrohWillowSyncCore {
    async fn addr(&self) -> Result<api::NodeAddr, SyncError> {
        let a = self
            .client
            .node_addr()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let direct_addresses = a
            .direct_addresses
            .iter()
            .map(|sa| api::DirectAddress::from(*sa))
            .collect::<Vec<_>>();
        let relay_url = a
            .relay_url
            .as_ref()
            .map(|u| api::RelayEndpoint::from(u.to_string()));
        Ok(api::NodeAddr {
            node_id: api::NodeId::from(format!("{}", a.node_id)),
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
        if let Some(first) = addr.direct_addresses.iter().next().copied() {
            let port = first.port();
            let loopback: SocketAddr = format!("127.0.0.1:{port}").parse()?;
            addr.direct_addresses.insert(loopback);
        }
        let direct_addresses = addr
            .direct_addresses
            .iter()
            .map(|sa| api::DirectAddress::from(*sa))
            .collect::<Vec<_>>();
        let relay_url = addr
            .relay_url
            .as_ref()
            .map(|u| api::RelayEndpoint::from(u.to_string()));
        Ok(api::SyncTicket {
            caps: caps
                .into_iter()
                .map(|c| serde_json::to_value(&c).unwrap_or(serde_json::json!({})))
                .collect(),
            nodes: vec![api::NodeAddr {
                node_id: api::NodeId::from(format!("{}", addr.node_id)),
                direct_addresses,
                relay_url,
            }],
        })
    }

    async fn import_and_sync(
        &self,
        ticket: api::SyncTicket,
        mode: api::SyncMode,
    ) -> Result<Box<dyn api::SyncHandle>, SyncError> {
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
            let relay_url = match &n.relay_url {
                Some(s) => Some(
                    RelayUrl::from_str(s.as_ref())
                        .map_err(|e| SyncError::InvalidId(e.to_string()))?,
                ),
                None => None,
            };
            nodes.push(IrohNodeAddr {
                node_id,
                direct_addresses: set,
                relay_url,
            });
        }
        let ticket = iroh_willow::rpc::client::SpaceTicket { caps, nodes };
        let mode = match mode {
            api::SyncMode::ReconcileOnce => iroh_willow::session::SessionMode::ReconcileOnce,
            api::SyncMode::Continuous => iroh_willow::session::SessionMode::Continuous,
        };
        let (space, mut handles) = self
            .client
            .import_and_sync(ticket, mode)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        // track imported provider namespace so list_namespaces includes it
        let ns = format!("{}", space.namespace_id());
        self.imported_namespaces
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
        Ok(Box::new(IrohWillowSyncHandle(Box::pin(s))))
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

#[allow(clippy::expect_used)]
impl IrohWillowSyncCore {
    pub(crate) async fn import_and_sync_concrete(
        &self,
        ticket: api::SyncTicket,
        mode: api::SyncMode,
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
            let relay_url = match &n.relay_url {
                Some(s) => Some(
                    RelayUrl::from_str(s.as_ref())
                        .map_err(|e| SyncError::InvalidId(e.to_string()))?,
                ),
                None => None,
            };
            nodes.push(IrohNodeAddr {
                node_id,
                direct_addresses: set,
                relay_url,
            });
        }
        let ticket = iroh_willow::rpc::client::SpaceTicket { caps, nodes };
        let mode = match mode {
            api::SyncMode::ReconcileOnce => iroh_willow::session::SessionMode::ReconcileOnce,
            api::SyncMode::Continuous => iroh_willow::session::SessionMode::Continuous,
        };
        let (space, mut handles) = self
            .client
            .import_and_sync(ticket, mode)
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let ns = format!("{}", space.namespace_id());
        self.imported_namespaces
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
}
