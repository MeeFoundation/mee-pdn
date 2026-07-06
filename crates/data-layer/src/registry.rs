//! Node-local addressing: the issuer-to-doc map data-namespace operations
//! resolve through.

use std::collections::HashMap;

use pdn_store::api::Doc;
use pdn_types::PdnId;

/// Node-local registry of data namespaces: issuer → backing doc, the map
/// `read`/`write`/`share_ticket` resolve through. Data namespaces of any
/// number of issuers coexist on one node.
///
/// The connections and private metadata docs are not kept here — they live
/// inside their store handles ([`ConnectionsStore`](crate::ConnectionsStore),
/// [`PrivateMetadataStore`](crate::PrivateMetadataStore)), whose holders know
/// which identity each serves.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    data_docs: HashMap<PdnId, Doc>,
}

impl Registry {
    /// Register the data namespace of `issuer` as backed by `doc`. Both
    /// created and imported namespaces register here.
    pub(crate) fn register_data(&mut self, issuer: PdnId, doc: Doc) {
        self.data_docs.insert(issuer, doc);
    }

    pub(crate) fn data_doc(&self, issuer: PdnId) -> Option<&Doc> {
        self.data_docs.get(&issuer)
    }
}
