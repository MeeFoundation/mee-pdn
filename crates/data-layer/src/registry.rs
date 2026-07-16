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
/// The connections and private metadata docs are not kept here — they live
/// inside their store handles ([`ConnectionsStore`](crate::ConnectionsStore),
/// [`PrivateMetadataStore`](crate::PrivateMetadataStore)), whose holders know
/// which identity each serves.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: RwLock<HashMap<PdnId, Doc>>,
}

impl Registry {
    /// Register the data namespace of `issuer` as backed by `doc`. Both
    /// created and imported namespaces register here.
    pub(crate) fn register_data(&self, issuer: PdnId, doc: Doc) -> Result<()> {
        self.data_docs
            .write()
            .map_err(|_poisoned| anyhow!("data registry lock poisoned"))?
            .insert(issuer, doc);
        Ok(())
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
