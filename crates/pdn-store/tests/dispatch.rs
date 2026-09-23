//! The docs protocol handler dispatches an accepted connection by the
//! identity its first message names, before any replica is touched, and
//! hands the streams to that identity's engine. What the session access
//! provider of that engine is asked carries the identity the caller
//! addressed and the identity the caller acts for.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint, PublicKey, Watcher as _};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol};
use iroh_gossip::net::Gossip;
use pdn_store::{
    api::{
        protocol::{AddrInfoOptions, ShareMode},
        Doc, DocsApi,
    },
    net::{connect_and_sync, AbortReason, ConnectError},
    protocol::{Docs, DocsDispatch, IdentityResolver},
    store::Query,
    Contact, Identity, NamespaceId, SessionAccess, SessionAccessFuture, SessionAccessProvider,
    SessionRole,
};

const WORK: Identity = Identity::from_bytes([0xa2; 32]);
const LEISURE: Identity = Identity::from_bytes([0xa3; 32]);
const CALLER: Identity = Identity::from_bytes([0xb0; 32]);
/// A identity no engine of the serving node answers for.
const UNHOSTED: Identity = Identity::from_bytes([0xc0; 32]);

const KEY: &[u8] = b"contact/email";
const WORK_VALUE: &[u8] = b"alice@work.example";
const LEISURE_VALUE: &[u8] = b"alice@leisure.example";

/// One question the provider was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Asked {
    namespace: NamespaceId,
    identity: Identity,
    caller: Identity,
    peer: PublicKey,
    role: SessionRole,
}

type Log = Arc<Mutex<Vec<Asked>>>;

/// A provider that serves whole and records what it was asked.
fn recording() -> (SessionAccessProvider, Log) {
    let log: Log = Arc::default();
    let recorded = Arc::clone(&log);
    let provider: SessionAccessProvider =
        Arc::new(move |namespace, identity, caller, peer, role| {
            if let Ok(mut asked) = recorded.lock() {
                asked.push(Asked {
                    namespace,
                    identity,
                    caller,
                    peer,
                    role,
                });
            }
            Box::pin(std::future::ready(SessionAccess::whole())) as SessionAccessFuture
        });
    (provider, log)
}

fn asked(log: &Log) -> Vec<Asked> {
    log.lock().expect("the recording lock").clone()
}

/// An endpoint on loopback, dialable: the scenario's two nodes reach
/// each other without a path off the machine, and an endpoint publishes
/// its first transport address a moment after it binds — a contact
/// taken before that names no path at all.
async fn loopback_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0u16))?
        .bind()
        .await?;
    while endpoint.watch_addr().get().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(endpoint)
}

/// A node of two hosted identities: two engines over one endpoint, the
/// docs protocol dispatching to whichever the connection names.
struct Serving {
    router: Router,
    work: Docs,
    leisure: Docs,
    work_log: Log,
    leisure_log: Log,
}

async fn serving_node() -> Result<Serving> {
    serving_node_routing(|named| named).await
}

/// `route` says whose engine the resolver answers a session naming an
/// identity with: that identity's own on a node that routes correctly.
async fn serving_node_routing(route: fn(Identity) -> Identity) -> Result<Serving> {
    let endpoint = loopback_endpoint().await?;
    let blobs = MemStore::new();
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let (work_provider, work_log) = recording();
    let (leisure_provider, leisure_log) = recording();
    let work = Docs::memory(WORK, work_provider)
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let leisure = Docs::memory(LEISURE, leisure_provider)
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let resolve: IdentityResolver = {
        let work = work.clone();
        let leisure = leisure.clone();
        Arc::new(move |identity| match route(identity) {
            h if h == WORK => Some(work.clone()),
            h if h == LEISURE => Some(leisure.clone()),
            _ => None,
        })
    };
    let router = Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
        .accept(iroh_gossip::ALPN, gossip)
        .accept(pdn_store::ALPN, DocsDispatch::new(resolve))
        .spawn();
    Ok(Serving {
        router,
        work,
        leisure,
        work_log,
        leisure_log,
    })
}

/// The dialing node: one engine, one identity, its own recording provider.
struct Dialing {
    router: Router,
    docs: Docs,
    log: Log,
}

async fn dialing_node() -> Result<Dialing> {
    let endpoint = loopback_endpoint().await?;
    let blobs = MemStore::new();
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let (provider, log) = recording();
    let docs = Docs::memory(CALLER, provider)
        .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
        .await?;
    let resolve: IdentityResolver = {
        let docs = docs.clone();
        Arc::new(move |identity| (identity == CALLER).then(|| docs.clone()))
    };
    let router = Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
        .accept(iroh_gossip::ALPN, gossip)
        .accept(pdn_store::ALPN, DocsDispatch::new(resolve))
        .spawn();
    Ok(Dialing { router, docs, log })
}

/// Write `value` into a fresh namespace of `api` and hand the capability
/// to `other`, so both sides hold the replica the session addresses.
async fn namespace_with(api: &DocsApi, other: &DocsApi, value: &[u8]) -> Result<(Doc, Doc)> {
    let doc = api.create().await?;
    let author = api.author_default().await?;
    doc.set_bytes(author, KEY.to_vec(), value.to_vec()).await?;
    let ticket = doc.share(ShareMode::Read, AddrInfoOptions::Id).await?;
    let theirs = other.import_namespace(ticket.capability).await?;
    Ok((doc, theirs))
}

/// Poll `check` until it holds or the budget runs out. The budget is
/// the suites' own: a session between two endpoints on loopback.
async fn eventually<F, Fut>(mut check: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if check().await? {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

async fn holds(doc: &Doc, value: &[u8]) -> Result<bool> {
    let Some(entry) = doc
        .get_one(Query::single_latest_per_key().key_exact(KEY))
        .await?
    else {
        return Ok(false);
    };
    Ok(entry.content_len() == value.len() as u64)
}

/// A node of two hosted identities is handed a session for each by the
/// identity its first message names, and each engine's provider is asked
/// about the identities that session carries — the one addressed and the
/// one the caller acts for.
///
/// Denied: a session naming a identity the node does not host is refused
/// exactly as one naming a replica the addressed identity does not hold,
/// and neither engine's provider is asked about it — the connection is
/// dispatched before any replica is touched.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_dispatched_by_the_identity_it_names() -> Result<()> {
    let serving = serving_node().await?;
    let dialing = dialing_node().await?;
    let serving_addr = serving.router.endpoint().addr();
    let dialing_id = dialing.router.endpoint().id();

    let (_work_doc, work_here) =
        namespace_with(serving.work.api(), dialing.docs.api(), WORK_VALUE).await?;
    let (_leisure_doc, leisure_here) =
        namespace_with(serving.leisure.api(), dialing.docs.api(), LEISURE_VALUE).await?;
    let work_namespace = work_here.id();
    let leisure_namespace = leisure_here.id();

    // Allowed: one session per hosted identity, over one endpoint.
    for (identity, doc, value) in [
        (WORK, &work_here, WORK_VALUE),
        (LEISURE, &leisure_here, LEISURE_VALUE),
    ] {
        // Contacts only: no swarm, so the only session either node can
        // open is the dial this scenario makes.
        doc.start_sync_scoped(vec![Contact::new(serving_addr.clone(), identity)], identity)
            .await?;
        assert!(
            eventually(|| holds(doc, value)).await?,
            "the session for {identity:?} carried nothing"
        );
    }

    // Each engine was asked about its own session alone, and about the
    // identities that session named. A pass may retry and the serving side
    // dials back, so the accepted questions are compared as a set.
    for (log, namespace, identity) in [
        (&serving.work_log, work_namespace, WORK),
        (&serving.leisure_log, leisure_namespace, LEISURE),
    ] {
        let mut distinct: Vec<Asked> = asked(log)
            .into_iter()
            .filter(|asked| asked.role == SessionRole::Accept)
            .collect();
        distinct.sort_by_key(|asked| (asked.namespace, asked.identity, asked.caller));
        distinct.dedup();
        assert_eq!(
            distinct,
            vec![Asked {
                namespace,
                identity,
                caller: CALLER,
                peer: dialing_id,
                role: SessionRole::Accept,
            }],
            "the engine of {identity:?} was not asked about exactly its own session"
        );
    }

    // Denied: a identity this node does not host, and a replica the
    // addressed identity does not hold, refuse the same way.
    let unhosted = connect_and_sync(
        dialing.router.endpoint(),
        &dialing.docs.engine().sync,
        work_namespace,
        UNHOSTED,
        CALLER,
        serving_addr.clone(),
        None,
        None,
        None,
    )
    .await
    .expect_err("a identity the node does not host must be refused");
    let not_held = connect_and_sync(
        dialing.router.endpoint(),
        &dialing.docs.engine().sync,
        leisure_namespace,
        WORK,
        CALLER,
        serving_addr.clone(),
        None,
        None,
        None,
    )
    .await
    .expect_err("a replica the addressed identity does not hold must be refused");
    for refusal in [&unhosted, &not_held] {
        assert!(
            matches!(refusal, ConnectError::RemoteAbort(AbortReason::NotFound)),
            "the refusal was not the not-found abort: {refusal:?}"
        );
    }

    // The unhosted identity never reached an engine: no question names it.
    for log in [&serving.work_log, &serving.leisure_log] {
        assert!(
            asked(log).iter().all(|asked| asked.identity != UNHOSTED),
            "a identity the node does not host reached an engine"
        );
    }

    serving.router.shutdown().await?;
    dialing.router.shutdown().await?;
    Ok(())
}

/// A session the resolver hands to another identity's engine is refused as
/// not hosted, and that engine's provider is never asked about it: a
/// routing defect costs the session, never a judgement by the records of
/// an identity the session did not name. The resolver here answers every
/// session with the first identity's engine; a session naming that
/// identity is served.
///
/// Denied: a session naming the co-located identity, for a replica the
/// first identity's engine holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_handed_to_another_identitys_engine_is_refused() -> Result<()> {
    let serving = serving_node_routing(|_named| WORK).await?;
    let dialing = dialing_node().await?;
    let serving_addr = serving.router.endpoint().addr();

    let (_work_doc, work_here) =
        namespace_with(serving.work.api(), dialing.docs.api(), WORK_VALUE).await?;
    work_here
        .start_sync_scoped(vec![Contact::new(serving_addr.clone(), WORK)], WORK)
        .await?;
    assert!(
        eventually(|| holds(&work_here, WORK_VALUE)).await?,
        "the session naming the routed identity carried nothing"
    );

    // Denied.
    let misrouted = connect_and_sync(
        dialing.router.endpoint(),
        &dialing.docs.engine().sync,
        work_here.id(),
        LEISURE,
        CALLER,
        serving_addr,
        None,
        None,
        None,
    )
    .await
    .expect_err("a session handed to another identity's engine must be refused");
    assert!(
        matches!(misrouted, ConnectError::RemoteAbort(AbortReason::NotFound)),
        "the refusal was not the not-found abort: {misrouted:?}"
    );
    assert!(
        asked(&serving.work_log)
            .iter()
            .all(|asked| asked.identity != LEISURE),
        "the misrouted session was judged by the records of an identity it did not name"
    );

    serving.router.shutdown().await?;
    dialing.router.shutdown().await?;
    Ok(())
}

/// The dialing side's provider is asked about the same pair of identities
/// under the dial role: the identity it addresses and the one it acts for.
#[tokio::test(flavor = "multi_thread")]
async fn the_dialing_side_is_asked_about_the_identity_it_addresses() -> Result<()> {
    let serving = serving_node().await?;
    let dialing = dialing_node().await?;
    let serving_addr = serving.router.endpoint().addr();

    let (_work_doc, work_here) =
        namespace_with(serving.work.api(), dialing.docs.api(), WORK_VALUE).await?;
    let namespace = work_here.id();

    work_here
        .start_sync_scoped(vec![Contact::new(serving_addr, WORK)], WORK)
        .await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let dial = loop {
        if let Some(dial) = asked(&dialing.log)
            .into_iter()
            .find(|asked| asked.role == SessionRole::Dial)
        {
            break dial;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dialing side's provider was never asked"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(dial.namespace, namespace);
    assert_eq!(
        dial.identity, WORK,
        "the dial did not address the contact's identity"
    );
    assert_eq!(
        dial.caller, CALLER,
        "the dial did not act as this node's identity"
    );

    serving.router.shutdown().await?;
    dialing.router.shutdown().await?;
    Ok(())
}
