use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use pdn_store::{filter::serve_whole, protocol::Docs, Holder, ALPN as DOCS_ALPN};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // create an iroh endpoint that includes the standard address lookup mechanisms
    // we've built at number0
    let endpoint = Endpoint::bind(presets::N0).await?;

    // build the blobs protocol
    let blobs = MemStore::default();

    // build the gossip protocol
    let gossip = Gossip::builder().spawn(endpoint.clone());

    // build the docs protocol: the holder every replica here is held for,
    // and what judges its sessions — `serve_whole` judging nothing
    let docs = Docs::memory(Holder::from_bytes([0u8; 32]), serve_whole())
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;

    // create a router builder, we will add the
    // protocols to this builder and then spawn
    // the router
    let builder = Router::builder(endpoint.clone());

    // setup router
    let _router = builder
        .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
        .accept(GOSSIP_ALPN, gossip)
        .accept(DOCS_ALPN, docs)
        .spawn();

    // do fun stuff with docs!
    Ok(())
}
