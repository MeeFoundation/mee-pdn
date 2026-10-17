use std::sync::{Arc, Mutex};

use anyhow::Result;
use iroh_gossip::net::Gossip;
use pdn_store::{
    api::Doc,
    filter::{SessionAccess, SessionAccessProvider},
    protocol::Docs,
};
use testresult::TestResult;
use tracing_test::traced_test;

use self::util::{empty_endpoint, Node, TEST_HOLDER};

mod util;

/// A `Doc` the engine's own access provider holds, as the data layer's
/// registry holds a pod's, fails its next call once the last `Docs` is
/// dropped. The test's own handle is dropped first, so the one inside the
/// provider is the only one left to keep the engine alive.
#[tokio::test]
#[traced_test]
async fn a_doc_the_access_provider_holds_stops_with_the_last_docs() -> Result<()> {
    let endpoint = empty_endpoint().await?;
    let blobs = iroh_blobs::store::mem::MemStore::new();
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let held: Arc<Mutex<Option<Doc>>> = Default::default();
    let access: SessionAccessProvider = {
        let held = held.clone();
        Arc::new(move |_namespace, _identity, _caller, _peer, _role| {
            let _held = &held;
            Box::pin(std::future::ready(SessionAccess::whole()))
        })
    };
    let docs = Docs::memory(TEST_HOLDER, access)
        .spawn(endpoint, (*blobs).clone(), gossip)
        .await?;
    let author = docs.author_default().await?;
    let doc = docs.create().await?;
    doc.set_bytes(author, "key", "before").await?;
    *held.lock().expect("lock") = Some(doc);

    drop(docs);
    let doc = held.lock().expect("lock").clone().expect("held");
    let err = doc
        .set_bytes(author, "key", "after")
        .await
        .expect_err("a call after the last Docs is dropped");
    assert_eq!(err.to_string(), "the store has stopped");
    Ok(())
}

/// A node's default author exists in its store: a write under it goes
/// through.
#[tokio::test]
async fn test_default_author_memory() -> Result<()> {
    let iroh = Node::memory(empty_endpoint().await?).spawn().await?;
    let author = iroh.docs().author_default().await?;
    let doc = iroh.docs().create().await?;
    doc.set_bytes(author, "key", "value").await?;
    Ok(())
}

/// A persistent node keeps its default author across restarts, creates a
/// new one when the default author file is gone, and refuses to start when
/// the file names an author its store does not hold.
#[tokio::test]
#[traced_test]
async fn test_default_author_persist() -> TestResult<()> {
    let iroh_root_dir = tempfile::TempDir::new()?;
    let iroh_root = iroh_root_dir.path();

    let default_author = {
        let iroh = Node::persistent(iroh_root, empty_endpoint().await?)
            .spawn()
            .await?;
        let author = iroh.docs().author_default().await?;
        let doc = iroh.docs().create().await?;
        doc.set_bytes(author, "key", "value").await?;
        iroh.shutdown().await?;
        author
    };

    {
        let iroh = Node::persistent(iroh_root, empty_endpoint().await?)
            .spawn()
            .await?;
        let author = iroh.docs().author_default().await?;
        assert_eq!(author, default_author);
        let doc = iroh.docs().create().await?;
        doc.set_bytes(author, "key", "value").await?;
        iroh.shutdown().await?;
    };

    let default_author = {
        tokio::fs::remove_file(iroh_root.join("default-author")).await?;
        let iroh = Node::persistent(iroh_root, empty_endpoint().await?)
            .spawn()
            .await?;
        let author = iroh.docs().author_default().await?;
        assert!(author != default_author);
        let doc = iroh.docs().create().await?;
        doc.set_bytes(author, "key", "value").await?;
        iroh.shutdown().await?;
        author
    };

    {
        let stranger = pdn_store::Author::new(&mut rand::rng()).id();
        assert!(stranger != default_author);
        tokio::fs::write(iroh_root.join("default-author"), stranger.to_string()).await?;
        let iroh = Node::persistent(iroh_root, empty_endpoint().await?)
            .spawn()
            .await;
        assert!(iroh.is_err());

        // On macOS the failed node's blob store lets go of its files late.
        #[cfg(target_os = "macos")]
        n0_future::time::sleep(std::time::Duration::from_secs(1)).await;

        tokio::fs::remove_file(iroh_root.join("default-author")).await?;
        drop(iroh);
        let iroh = Node::persistent(iroh_root, empty_endpoint().await?)
            .spawn()
            .await;
        if let Err(cause) = iroh.as_ref() {
            panic!("failed to start node: {cause:?}");
        }
        iroh?.shutdown().await?;
    }

    Ok(())
}
