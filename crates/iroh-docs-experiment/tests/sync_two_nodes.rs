//! Minimal end-to-end: two in-process iroh nodes sync one key in one doc.

use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_docs::{
    api::protocol::{AddrInfoOptions, ShareMode},
    protocol::Docs,
    store::Query,
    ALPN as DOCS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};

struct Node {
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: iroh_docs::api::DocsApi,
}

async fn spawn_node() -> Result<Node> {
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
        router,
        blobs: blobs_store,
        docs: docs_api,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_two_nodes() -> Result<()> {
    let node_a = spawn_node().await?;
    let node_b = spawn_node().await?;

    let author = node_a.docs.author_create().await?;
    let doc_a = node_a.docs.create().await?;
    doc_a
        .set_bytes(author, b"k1".to_vec(), b"v1".to_vec())
        .await?;

    let ticket = doc_a
        .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let doc_b = node_b.docs.import(ticket).await?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let value = loop {
        if let Some(entry) = doc_b
            .get_one(Query::single_latest_per_key().key_exact(b"k1"))
            .await?
        {
            let bytes = node_b.blobs.get_bytes(entry.content_hash()).await?;
            break bytes.to_vec();
        }
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for sync");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(value, b"v1");

    node_a.router.shutdown().await?;
    node_b.router.shutdown().await?;
    Ok(())
}
