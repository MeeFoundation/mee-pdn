//! Node-local addressing: the issuer-to-doc map data-namespace operations
//! resolve through.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{anyhow, Result};
use pdn_store::api::Doc;
use pdn_types::PdnId;

/// Node-local registry of data namespaces: issuer → backing doc, the map
/// `read`/`write`/`share_ticket` resolve through. Data namespaces of any
/// number of issuers coexist on one node.
///
/// Interior-mutable (`RwLock`) so registration takes `&self`: a node's
/// doc-creating operations then need no exclusive access, and the runtime's
/// coarse lock is never held for the registry's sake — only for its own
/// in-place state. `data_doc` hands back a cloned [`Doc`] (a cheap handle),
/// not a borrow, so no read guard escapes.
///
/// The private metadata and connection metadata docs are not kept here —
/// they live inside their store handles
/// ([`PrivateMetadataStore`](crate::PrivateMetadataStore),
/// [`ConnectionMetadataStore`](crate::ConnectionMetadataStore)), whose
/// holders know which identity each serves.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: RwLock<HashMap<PdnId, Doc>>,
}

impl Registry {
    /// Register the data namespace of `issuer` as backed by `doc`, handing
    /// back the registration this replaced (`None` if the issuer was
    /// unbound). Both created and imported namespaces register here.
    ///
    /// The displaced doc is returned rather than dropped on the floor so a
    /// caller that must be undoable can put it back: an act that replaced a
    /// binding it did not create must restore it on failure, never delete
    /// it. Discarding the return is a deliberate choice at each call site,
    /// not the default.
    #[must_use = "the displaced registration must be restored or knowingly discarded"]
    pub(crate) fn register_data(&self, issuer: PdnId, doc: Doc) -> Result<Option<Doc>> {
        Ok(self
            .data_docs
            .write()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .insert(issuer, doc))
    }

    /// Remove the registration of `issuer`'s data namespace, handing back
    /// the doc it resolved to (`None` if the issuer was not registered).
    /// The unregister half of
    /// [`SyncNode::forget_namespace`](crate::SyncNode::forget_namespace) —
    /// the caller drops the replica.
    pub(crate) fn unregister_data(&self, issuer: PdnId) -> Result<Option<Doc>> {
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
            .cloned())
    }
}
