//! Capability-gated end-to-end sync over iroh-docs.
//!
//! Two in-process iroh nodes. Bob accepts an incoming entry only if the entry's
//! namespace owner (resolved via `owners`) is in his `connections` set — the
//! simplified, single-link form of a capability chain, injected into the
//! local iroh-docs variant through `Docs::memory().capability_validator(..)`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_docs::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc,
    },
    protocol::Docs,
    store::Query,
    AuthorId, CapabilityValidator, DocTicket, NamespaceId as IrohNamespaceId, SignedEntry,
    ALPN as DOCS_ALPN,
};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use mee_sync_api::NamespaceId;
use mee_types::MeeId;

/// A node's live set of connections: the `MeeId`s it currently accepts data from.
type Connections = Arc<RwLock<HashSet<MeeId>>>;

/// Resolver: iroh doc namespace -> the `MeeId` that issued it (`issued_by`).
type Owners = Arc<RwLock<HashMap<IrohNamespaceId, MeeId>>>;

struct Node {
    /// The Mee identity that owns this node.
    owner: MeeId,
    router: Router,
    blobs: iroh_blobs::api::Store,
    docs: iroh_docs::api::DocsApi,
    /// Maps a domain `NamespaceId` to the iroh `Doc` that backs it.
    namespaces: HashMap<NamespaceId, Doc>,
    /// `MeeIds` this node accepts incoming entries from (mutated at runtime).
    connections: Connections,
    /// iroh-namespace -> issuer `MeeId`, populated when a namespace is imported.
    owners: Owners,
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
        // Teach the validator who owns this doc, so an incoming entry's iroh
        // namespace resolves to the issuer MeeId it must be connected to.
        self.owners
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(doc.id(), id.issued_by);
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

    let connections: Connections = Arc::new(RwLock::new(HashSet::new()));
    let owners: Owners = Arc::new(RwLock::new(HashMap::new()));

    // Capability gate: accept an incoming entry iff its namespace owner is a
    // current connection. This is the injected UWill seam (Tier 1, naive form).
    let validator: CapabilityValidator = {
        let connections = connections.clone();
        let owners = owners.clone();
        Arc::new(move |entry: &SignedEntry| {
            let ns = entry.entry().namespace();
            let owner = owners
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&ns)
                .copied();
            match owner {
                Some(meeid) => connections
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&meeid),
                None => false,
            }
        })
    };

    let docs = Docs::memory()
        .capability_validator(validator)
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
        connections,
        owners,
    })
}

async fn write_key(doc: &Doc, author: AuthorId, key: &str, value: &[u8]) -> Result<()> {
    doc.set_bytes(author, key.as_bytes().to_vec(), value.to_vec())
        .await?;
    Ok(())
}

async fn read_key(blobs: &iroh_blobs::api::Store, doc: &Doc, key: &str) -> Result<Option<Vec<u8>>> {
    match doc
        .get_one(Query::single_latest_per_key().key_exact(key.as_bytes()))
        .await?
    {
        Some(entry) => Ok(Some(blobs.get_bytes(entry.content_hash()).await?.to_vec())),
        None => Ok(None),
    }
}

/// Poll until `key` is present, or return `None` once `timeout` elapses.
async fn wait_for_key(
    blobs: &iroh_blobs::api::Store,
    doc: &Doc,
    key: &str,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = read_key(blobs, doc, key).await? {
            return Ok(Some(value));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn capability_gated_sync() -> Result<()> {
    // Mee identities.
    let carol = MeeId::from_bytes([0xca; 32]);
    let alice = MeeId::from_bytes([0xa1; 32]);
    let bob = MeeId::from_bytes([0xb0; 32]);

    let mut alice_node = spawn_node(alice).await?;
    let mut bob_node = spawn_node(bob).await?;

    // Namespace about Carol, issued by Alice (the sole writer/owner).
    let namespace = NamespaceId::new(carol, alice);
    assert_eq!(namespace.issued_by, alice_node.owner);

    let alice_author = alice_node.docs.author_create().await?;
    let alice_doc = alice_node.create_namespace(namespace).await?;

    // Alice writes k1 before Bob connects: this entry reaches Bob via the
    // initial set-reconciliation path.
    write_key(&alice_doc, alice_author, "k1", b"v1").await?;

    let ticket = alice_doc
        .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    bob_node.import_namespace(namespace, ticket).await?;
    let bob_doc = bob_node
        .doc(&namespace)
        .context("namespace registered on bob_node")?;

    // Step 1 — Alice is NOT among Bob's connections: the gate drops k1.
    let got = wait_for_key(&bob_node.blobs, &bob_doc, "k1", Duration::from_secs(3)).await?;
    assert!(
        got.is_none(),
        "k1 synced even though Alice is not a connection"
    );

    // Step 2 — add Alice as a connection; a fresh write (k2) must now sync
    // (live-gossip path -> insert_entry -> validate_entry -> accept).
    bob_node
        .connections
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(alice);
    write_key(&alice_doc, alice_author, "k2", b"v2").await?;
    let got = wait_for_key(&bob_node.blobs, &bob_doc, "k2", Duration::from_secs(15)).await?;
    assert_eq!(
        got.as_deref(),
        Some(b"v2".as_ref()),
        "k2 did not sync after connecting Alice"
    );

    // Step 3 — revoke Alice (remove from connections); a fresh write (k3) is
    // rejected again, symmetric to step 1.
    bob_node
        .connections
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&alice);
    write_key(&alice_doc, alice_author, "k3", b"v3").await?;
    let got = wait_for_key(&bob_node.blobs, &bob_doc, "k3", Duration::from_secs(3)).await?;
    assert!(got.is_none(), "k3 synced even though Alice was revoked");

    alice_node.router.shutdown().await?;
    bob_node.router.shutdown().await?;
    Ok(())
}
