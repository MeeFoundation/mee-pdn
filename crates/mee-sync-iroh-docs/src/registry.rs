//! Binding between domain namespaces and the iroh docs that back them.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use iroh_docs::{api::Doc, NamespaceId as IrohNamespaceId};
use mee_sync_api::NamespaceId;

/// Shared, synchronously readable map: iroh namespace → domain namespace.
///
/// Written when namespaces are created or imported; read by the ingest gate
/// on the sync-actor thread to resolve incoming entries to domain terms.
#[derive(Clone, Debug, Default)]
pub(crate) struct NamespaceIndex {
    inner: Arc<RwLock<HashMap<IrohNamespaceId, NamespaceId>>>,
}

impl NamespaceIndex {
    fn bind(&self, iroh_ns: IrohNamespaceId, domain_ns: NamespaceId) {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(iroh_ns, domain_ns);
    }

    pub(crate) fn resolve(&self, iroh_ns: IrohNamespaceId) -> Option<NamespaceId> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&iroh_ns)
            .copied()
    }
}

/// Node-local registry: domain namespace → backing doc, plus the shared
/// [`NamespaceIndex`] the gate reads.
#[derive(Debug)]
pub(crate) struct Registry {
    index: NamespaceIndex,
    docs: HashMap<NamespaceId, Doc>,
}

impl Registry {
    pub(crate) fn new(index: NamespaceIndex) -> Self {
        Self {
            index,
            docs: HashMap::new(),
        }
    }

    /// Bind `domain_ns` to `doc`, teaching the gate's index the reverse
    /// mapping. Both created and imported namespaces register here, so the
    /// gate can resolve remote entries regardless of which side opened the
    /// doc first.
    pub(crate) fn bind(&mut self, domain_ns: NamespaceId, doc: Doc) {
        self.index.bind(doc.id(), domain_ns);
        self.docs.insert(domain_ns, doc);
    }

    pub(crate) fn doc(&self, domain_ns: &NamespaceId) -> Option<&Doc> {
        self.docs.get(domain_ns)
    }
}
