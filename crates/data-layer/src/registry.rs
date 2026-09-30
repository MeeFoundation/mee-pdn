//! Node-local addressing: the issuer-to-doc map data-namespace operations
//! resolve through, and the cell-to-stores map cell operations resolve
//! through.

use std::{collections::HashMap, sync::RwLock};

use anyhow::{anyhow, Result};
use pdn_store::{api::Doc, NamespaceId};
use pdn_types::{CellId, PdnId};

/// How an identity serves a data replica it holds. Independent of the sync
/// strategy (swarm vs contacts-only), which lives on the tracked doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServingPosture {
    /// As the issuer's own: the identity's devices see it whole, a
    /// counterparty what the identity granted it.
    Serve,
    /// Grantee: the issuer's published devices see it whole, the identity's
    /// own devices the slice its grant record covers; everyone else is
    /// refused, their rights not being computable here.
    AudienceDevices,
}

/// One issuer's data-namespace binding: the backing doc and the serving
/// posture its sessions are classified under.
#[derive(Debug, Clone)]
pub(crate) struct DataBinding {
    pub(crate) doc: Doc,
    pub(crate) posture: ServingPosture,
}

/// Which of a cell's two stores a namespace is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellStore {
    Membership,
    Records,
}

/// One cell as one identity holds it. `records` is `None` once the identity
/// departed: the membership store is then the cell's tombstone.
#[derive(Debug, Clone)]
pub(crate) struct CellBinding {
    pub(crate) membership: Doc,
    pub(crate) records: Option<Doc>,
}

impl CellBinding {
    fn store_of(&self, namespace: NamespaceId) -> Option<CellStore> {
        if self.membership.id() == namespace {
            return Some(CellStore::Membership);
        }
        self.records
            .as_ref()
            .filter(|records| records.id() == namespace)
            .map(|_records| CellStore::Records)
    }
}

/// Node-local registry of data namespaces — issuer → backing doc — and of
/// cells — cell id → its two stores. Lookups hand back cloned [`Doc`]s (cheap
/// handles), so no read guard escapes. The metadata docs are not kept here —
/// they live inside their store handles, and the access book registers the
/// ones classification needs. Where both maps are locked, `data_docs` is
/// taken first.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: RwLock<HashMap<PdnId, DataBinding>>,
    cells: RwLock<HashMap<CellId, CellBinding>>,
}

impl Registry {
    /// Register the data namespace of `issuer` as backed by `doc`, handing
    /// back the binding this replaced (`None` if the issuer was unbound).
    /// A displaced binding is dropped with the replica it names, never put
    /// back: what an undo restores is what the identity held before, and
    /// this registry holds one identity's bindings alone (ADR-0013).
    ///
    /// One namespace binds one issuer: registering a second issuer onto a
    /// replica another one is bound to is refused, because the reverse
    /// lookup ([`binding_of`](Self::binding_of)) would otherwise pick between
    /// the two arbitrarily and could answer with the wrong serving posture —
    /// a fail-open branch.
    pub(crate) fn register_data(
        &self,
        issuer: PdnId,
        doc: Doc,
        posture: ServingPosture,
    ) -> Result<Option<DataBinding>> {
        let binding = DataBinding { doc, posture };
        let mut docs = self
            .data_docs
            .write()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?;
        let namespace = binding.doc.id();
        if let Some((other, _binding)) = docs
            .iter()
            .find(|(other, b)| **other != issuer && b.doc.id() == namespace)
        {
            return Err(anyhow!(
                "namespace {namespace} is already bound to issuer {other}; \
                 one namespace binds one issuer"
            ));
        }
        if let Some((cell, _store)) = cell_of(&*self.read_cells()?, namespace) {
            return Err(anyhow!(
                "namespace {namespace} is a store of cell {cell}; it binds no issuer"
            ));
        }
        Ok(docs.insert(issuer, binding))
    }

    /// Remove the registration of `issuer`'s data namespace, handing back
    /// the binding it resolved to (`None` if the issuer was not registered).
    pub(crate) fn unregister_data(&self, issuer: PdnId) -> Result<Option<DataBinding>> {
        Ok(self
            .data_docs
            .write()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .remove(&issuer))
    }

    pub(crate) fn data_doc(&self, issuer: PdnId) -> Result<Option<Doc>> {
        Ok(self
            .data_docs
            .read()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .get(&issuer)
            .map(|binding| binding.doc.clone()))
    }

    /// The full binding of `issuer` — doc plus serving posture.
    pub(crate) fn binding(&self, issuer: PdnId) -> Result<Option<DataBinding>> {
        Ok(self
            .data_docs
            .read()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .get(&issuer)
            .cloned())
    }

    /// Reverse lookup for session classification: which issuer `namespace`
    /// is bound to on this node, and under which serving posture.
    pub(crate) fn binding_of(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<(PdnId, ServingPosture)>> {
        Ok(self
            .data_docs
            .read()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .iter()
            .find(|(_issuer, binding)| binding.doc.id() == namespace)
            .map(|(issuer, binding)| (*issuer, binding.posture)))
    }

    /// Register `cell` as held on `binding`'s stores. Refused for a cell
    /// already registered, and for a store another role holds: a data
    /// namespace, another cell's store, or the cell's other store.
    pub(crate) fn register_cell(&self, cell: CellId, binding: CellBinding) -> Result<()> {
        let data = self
            .data_docs
            .read()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?;
        let mut cells = self
            .cells
            .write()
            .map_err(|_poisoned| anyhow!("cell registry lock poisoned"))?;
        if cells.contains_key(&cell) {
            return Err(anyhow!("cell {cell} is already registered"));
        }
        let namespaces = std::iter::once(binding.membership.id())
            .chain(binding.records.as_ref().map(Doc::id))
            .collect::<Vec<_>>();
        if let [membership, records] = namespaces.as_slice() {
            if membership == records {
                return Err(anyhow!(
                    "namespace {membership} cannot be both stores of cell {cell}"
                ));
            }
        }
        for namespace in namespaces {
            if data.values().any(|bound| bound.doc.id() == namespace) {
                return Err(anyhow!(
                    "namespace {namespace} is a data namespace; it is no store of cell {cell}"
                ));
            }
            if let Some((other, _store)) = cell_of(&cells, namespace) {
                return Err(anyhow!(
                    "namespace {namespace} is a store of cell {other}; it is no store of cell {cell}"
                ));
            }
        }
        cells.insert(cell, binding);
        Ok(())
    }

    /// Replace `cell`'s record store: `None` turns the cell into its
    /// tombstone, a doc turns the tombstone back into the cell. Answers
    /// whether `cell` was registered.
    pub(crate) fn set_cell_records(&self, cell: CellId, records: Option<Doc>) -> Result<bool> {
        let mut cells = self
            .cells
            .write()
            .map_err(|_poisoned| anyhow!("cell registry lock poisoned"))?;
        let Some(binding) = cells.get_mut(&cell) else {
            return Ok(false);
        };
        binding.records = records;
        Ok(true)
    }

    pub(crate) fn cell(&self, cell: CellId) -> Result<Option<CellBinding>> {
        Ok(self.read_cells()?.get(&cell).cloned())
    }

    /// Reverse lookup: which cell `namespace` is a store of, and which store.
    pub(crate) fn cell_of(&self, namespace: NamespaceId) -> Result<Option<(CellId, CellStore)>> {
        Ok(cell_of(&*self.read_cells()?, namespace))
    }

    fn read_cells(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<CellId, CellBinding>>> {
        self.cells
            .read()
            .map_err(|_poisoned| anyhow!("cell registry lock poisoned"))
    }
}

fn cell_of(
    cells: &HashMap<CellId, CellBinding>,
    namespace: NamespaceId,
) -> Option<(CellId, CellStore)> {
    cells
        .iter()
        .find_map(|(cell, binding)| binding.store_of(namespace).map(|store| (*cell, store)))
}
