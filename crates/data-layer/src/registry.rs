//! Node-local addressing: the issuer-to-doc map data-namespace operations
//! resolve through.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{anyhow, Result};
use pdn_store::api::Doc;
use pdn_store::NamespaceId;
use pdn_types::PdnId;

/// How this node serves a data replica whose issuer it does not host.
/// `Serve` is the ticket-bounded stance: the whole replica to any ticket
/// holder — the stance a device replicating a store re-serves the next
/// device under. `AudienceDevices` is a grantee's stance: the slice is
/// served to the devices of the grant's audience identity, judged through
/// that identity's directory and the locally replicated grant record;
/// third parties are refused — their rights are not computable here. This
/// axis is independent of the sync strategy (swarm vs contacts-only),
/// which lives on the tracked doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServingPosture {
    /// Ticket-bounded: serve the whole replica to any ticket holder.
    Serve,
    /// Grantee (scoped or whole-store): serve the audience identity's
    /// devices per the local grant record; refuse everyone else.
    AudienceDevices,
}

/// One issuer's data-namespace binding: the backing doc and the serving
/// posture the access book classifies its sessions under. The sync
/// strategy is deliberately not here — it lives on the tracked doc, so the
/// two axes can be set independently.
#[derive(Debug, Clone)]
pub(crate) struct DataBinding {
    pub(crate) doc: Doc,
    pub(crate) posture: ServingPosture,
}

/// Node-local registry of data namespaces: issuer → backing doc, the map
/// `read`/`write`/`share_ticket` resolve through. Data namespaces of any
/// number of issuers coexist on one node.
///
/// Interior-mutable (`RwLock`) so registration takes `&self`; `data_doc`
/// hands back a cloned [`Doc`] (a cheap handle), not a borrow, so no read
/// guard escapes. The private and connection metadata docs are not kept
/// here — they live inside their store handles; the access book registers
/// the ones session classification needs.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: RwLock<HashMap<PdnId, DataBinding>>,
}

impl Registry {
    /// Register the data namespace of `issuer` as backed by `doc`, handing
    /// back the binding this replaced (`None` if the issuer was unbound).
    /// The displaced binding is returned rather than dropped so an undoable
    /// caller can put it back; discarding it is a deliberate choice at each
    /// call site.
    #[must_use = "the displaced registration must be restored or knowingly discarded"]
    pub(crate) fn register_data(
        &self,
        issuer: PdnId,
        doc: Doc,
        posture: ServingPosture,
    ) -> Result<Option<DataBinding>> {
        self.register_binding(issuer, DataBinding { doc, posture })
    }

    /// Register a binding wholesale — the restore half of an undo, which
    /// must put back exactly what it displaced, serving posture included.
    ///
    /// One namespace binds one issuer: registering a second identity onto a
    /// replica another one is bound to is refused — the reverse lookup
    /// ([`binding_of`](Self::binding_of)) would otherwise pick between the
    /// two arbitrarily and could answer with the wrong serving posture, a
    /// fail-open branch.
    #[must_use = "the displaced registration must be restored or knowingly discarded"]
    pub(crate) fn register_binding(
        &self,
        issuer: PdnId,
        binding: DataBinding,
    ) -> Result<Option<DataBinding>> {
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
    /// The unregister half of
    /// [`SyncNode::forget_namespace`](crate::SyncNode::forget_namespace) —
    /// the caller drops the replica.
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
