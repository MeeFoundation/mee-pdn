//! Binding between domain replicas — data namespaces and the connections
//! store — and the iroh docs that back them.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use pdn_store::{api::Doc, NamespaceId as IrohNamespaceId};
use pdn_types::PdnId;

/// What an iroh replica is, in domain terms.
///
/// The gate resolves this from an incoming entry's iroh namespace so a
/// policy can decide structurally — a peer-visible data namespace vs. a
/// device-shared private store. The enum can grow when cross-party metadata
/// channels arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A peer-visible data namespace, keyed by its `issuer`.
    Data { issuer: PdnId },
    /// The device-shared connections store of `identity`.
    Connections { identity: PdnId },
    /// The device-shared private metadata store of `identity` (devices, tickets).
    PrivateMetadata { identity: PdnId },
}

/// Shared, synchronously readable map: iroh namespace → [`Binding`].
///
/// Written when replicas are created or imported; read by the ingest gate
/// on the sync-actor thread to resolve incoming entries to domain terms.
#[derive(Clone, Debug, Default)]
pub(crate) struct BindingIndex {
    inner: Arc<RwLock<HashMap<IrohNamespaceId, Binding>>>,
}

impl BindingIndex {
    fn bind(&self, iroh_ns: IrohNamespaceId, binding: Binding) {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(iroh_ns, binding);
    }

    pub(crate) fn resolve(&self, iroh_ns: IrohNamespaceId) -> Option<Binding> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&iroh_ns)
            .copied()
    }
}

/// Node-local registry: data namespace → backing doc, plus the shared
/// [`BindingIndex`] the gate reads.
///
/// The connections store's doc is held by its `ConnectionsStore`, not here;
/// the registry only needs to teach the gate its binding.
#[derive(Debug)]
pub(crate) struct Registry {
    index: BindingIndex,
    data_docs: HashMap<PdnId, Doc>,
}

impl Registry {
    pub(crate) fn new(index: BindingIndex) -> Self {
        Self {
            index,
            data_docs: HashMap::new(),
        }
    }

    /// Bind the data namespace of `issuer` to `doc`, teaching the gate the
    /// reverse mapping. Both created and imported namespaces register here, so
    /// the gate can resolve remote entries regardless of which side opened the
    /// doc first.
    pub(crate) fn bind_data(&mut self, issuer: PdnId, doc: Doc) {
        self.index.bind(doc.id(), Binding::Data { issuer });
        self.data_docs.insert(issuer, doc);
    }

    /// Bind the connections store of `identity`, teaching the gate the reverse
    /// mapping. The backing doc lives in the `ConnectionsStore`.
    pub(crate) fn bind_connections(&mut self, identity: PdnId, doc: &Doc) {
        self.index.bind(doc.id(), Binding::Connections { identity });
    }

    /// Bind the private metadata store of `identity`, teaching the gate the
    /// reverse mapping. The backing doc lives in the `PrivateMetadataStore`.
    pub(crate) fn bind_private_metadata(&mut self, identity: PdnId, doc: &Doc) {
        self.index
            .bind(doc.id(), Binding::PrivateMetadata { identity });
    }

    pub(crate) fn data_doc(&self, issuer: PdnId) -> Option<&Doc> {
        self.data_docs.get(&issuer)
    }
}
