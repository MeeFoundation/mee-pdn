//! Minimal end-to-end: two in-process iroh nodes sync one key in one doc.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_docs::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    protocol::Docs,
    store::Query,
    DocTicket, ALPN as DOCS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use mee_sync_api::{EntryPath, NamespaceId};
use mee_types::MeeId;

struct Node {
    /// The Mee identity that owns this node.
    owner: MeeId,
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: iroh_docs::api::DocsApi,
    /// Maps a domain `NamespaceId` to the iroh `Doc` that backs it.
    namespaces: HashMap<NamespaceId, Doc>,
}

impl Node {
    /// Create a fresh iroh doc and bind it to a domain namespace id.
    async fn create_namespace(&mut self, id: NamespaceId) -> Result<Doc> {
        let doc = self.docs.create().await?;
        self.namespaces.insert(id, doc.clone());
        Ok(doc)
    }

    /// Import a doc shared via ticket and bind it to a domain namespace id.
    async fn import_namespace(&mut self, id: NamespaceId, ticket: DocTicket) -> Result<()> {
        let doc = self.docs.import(ticket).await?;
        self.namespaces.insert(id, doc);
        Ok(())
    }

    /// Look up the iroh doc backing a domain namespace.
    fn doc(&self, id: &NamespaceId) -> Option<Doc> {
        self.namespaces.get(id).cloned()
    }
}

async fn spawn_node(owner: MeeId) -> Result<Node> {
    let endpoint = Endpoint::bind(presets::Minimal).await?;
    let blobs = MemStore::default();
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let docs = Docs::memory()
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let docs_api = docs.api().clone();
    let blobs_store: iroh_blobs::api::Store = (*blobs).clone();
    let router = Router::builder(endpoint.clone())
        .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
        .accept(GOSSIP_ALPN, gossip)
        .accept(DOCS_ALPN, docs)
        .spawn();
    Ok(Node {
        owner,
        router,
        blobs: blobs_store,
        docs: docs_api,
        namespaces: HashMap::new(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_two_nodes() -> Result<()> {
    // Mee identities.
    let carol = MeeId::from_bytes([0xca; 32]);
    let alice = MeeId::from_bytes([0xa1; 32]);
    let bob = MeeId::from_bytes([0xb0; 32]);

    let mut alice_node = spawn_node(alice).await?;
    let mut bob_node = spawn_node(bob).await?;

    // Namespace about Carol, issued by Alice (the sole writer/owner); the
    // random iroh NamespaceId stays an internal backing-store detail.
    let namespace = NamespaceId::new(carol, alice);
    // Alice issues into the namespace she owns.
    assert_eq!(namespace.issued_by, alice_node.owner);

    let alice_author = alice_node.docs.author_create().await?;
    let alice_doc = alice_node.create_namespace(namespace).await?;
    let path = EntryPath::new("k1")?;
    alice_doc
        .set_bytes(
            alice_author,
            path.as_str().as_bytes().to_vec(),
            b"v1".to_vec(),
        )
        .await?;

    let ticket = alice_doc
        .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    // Bob's node binds the same domain namespace to the doc it imports.
    bob_node.import_namespace(namespace, ticket).await?;
    let bob_doc = bob_node
        .doc(&namespace)
        .expect("namespace registered on bob_node");

    let deadline = Instant::now() + Duration::from_secs(30);
    let value = loop {
        if let Some(entry) = bob_doc
            .get_one(Query::single_latest_per_key().key_exact(path.as_str().as_bytes()))
            .await?
        {
            let bytes = bob_node.blobs.get_bytes(entry.content_hash()).await?;
            break bytes.to_vec();
        }
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for sync");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(value, b"v1");

    alice_node.router.shutdown().await?;
    bob_node.router.shutdown().await?;
    Ok(())
}
