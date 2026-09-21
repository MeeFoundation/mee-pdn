//! Node-local addressing: the issuer-to-doc map data-namespace operations
//! resolve through.

use std::{collections::HashMap, sync::RwLock};

use anyhow::{anyhow, Result};
use pdn_store::{api::Doc, NamespaceId};
use pdn_types::PdnId;

/// How this node serves a data replica whose issuer it does not host.
/// Independent of the sync strategy (swarm vs contacts-only), which lives
/// on the tracked doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServingPosture {
    /// Ticket-bounded: the whole replica to any ticket holder — the stance a
    /// device replicating a store re-serves the next device under.
    Serve,
    /// Grantee: the slice is served to the devices of the grant's audience
    /// identity, judged through that identity's directory and the locally
    /// replicated grant record; everyone else is refused, their rights not
    /// being computable here.
    AudienceDevices,
}

/// One issuer's data-namespace binding: the backing doc and the serving
/// posture its sessions are classified under.
#[derive(Debug, Clone)]
pub(crate) struct DataBinding {
    pub(crate) doc: Doc,
    pub(crate) posture: ServingPosture,
}

/// Node-local registry of data namespaces: issuer → backing doc. `data_doc`
/// hands back a cloned [`Doc`] (a cheap handle), so no read guard escapes.
/// The metadata docs are not kept here — they live inside their store
/// handles, and the access book registers the ones classification needs.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: RwLock<HashMap<PdnId, DataBinding>>,
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
}
