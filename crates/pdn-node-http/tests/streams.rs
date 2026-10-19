//! The bounds of the observation routes' event streams, on runtimes of this
//! process: holding streams and requests in flight together takes a held
//! state lock, which no request on the surface can produce, and the stop
//! signal is the host's own.
#![cfg(feature = "observation")]

use std::{sync::Arc, time::Duration};

use anyhow::{ensure, Context as _, Result};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use pdn_node::{IdentityService as _, PdnId, PodId, PodsService as _, Runtime, SpawnOptions};
use pdn_node_http::{
    router, serve, GRACEFUL_DRAIN_BUDGET, MAX_CONCURRENT_REQUESTS, MAX_OPEN_STREAMS, POD_HEADER,
};
use tower::ServiceExt as _;

/// What an answer takes from a router whose handler need not wait on the
/// runtime.
const PROMPT: Duration = Duration::from_secs(1);

async fn hosting_a_pod() -> Result<(Arc<Runtime>, PdnId, PodId)> {
    let runtime = Arc::new(Runtime::spawn(SpawnOptions::memory()).await?);
    let identity = runtime.identity().create().await?;
    let pod = runtime.pods().create(identity).await?;
    Ok((runtime, identity, pod))
}

async fn open_events(app: &Router, identity: PdnId, pod: PodId) -> Result<Response> {
    let request = Request::get(format!("/debug/identities/{identity}/pods/{pod}/events"))
        .body(Body::empty())?;
    Ok(app.clone().oneshot(request).await?)
}

/// `count` event streams, each answered 200 and held open.
async fn open_streams(
    app: &Router,
    identity: PdnId,
    pod: PodId,
    count: usize,
) -> Result<Vec<Response>> {
    let mut streams = Vec::with_capacity(count);
    for _ in 0..count {
        let stream = open_events(app, identity, pod).await?;
        ensure!(stream.status() == StatusCode::OK, "{}", stream.status());
        streams.push(stream);
    }
    Ok(streams)
}

/// Holds `runtime`'s state lock until the returned release is notified.
async fn hold_state_lock(
    runtime: &Arc<Runtime>,
) -> Result<(Arc<tokio::sync::Notify>, tokio::task::JoinHandle<()>)> {
    let acquired = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let holder = {
        let (runtime, acquired, release) = (
            Arc::clone(runtime),
            Arc::clone(&acquired),
            Arc::clone(&release),
        );
        tokio::spawn(async move { runtime.hold_state_lock_for_test(acquired, release).await })
    };
    tokio::time::timeout(Duration::from_secs(5), acquired.notified())
        .await
        .context("the state lock was not acquired")?;
    Ok((release, holder))
}

/// With every stream slot taken, the 16 request permits are all still
/// free: 16 requests waiting on the held state lock are admitted. Paired
/// denial: the request past them is shed, so the permits are the 16 and no
/// more. Fails with a stream holding its request permit past its headers.
#[tokio::test(flavor = "multi_thread")]
async fn open_streams_leave_the_request_bound_free() -> Result<()> {
    let (runtime, identity, pod) = hosting_a_pod().await?;
    let app = router(Arc::clone(&runtime), true);
    let streams = open_streams(&app, identity, pod, MAX_OPEN_STREAMS).await?;
    let (release, holder) = hold_state_lock(&runtime).await?;

    let requests: Vec<_> = (0..=MAX_CONCURRENT_REQUESTS)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                let request = Request::get("/debug/identities").body(Body::empty())?;
                anyhow::Ok(app.oneshot(request).await?.status())
            })
        })
        .collect();
    let mut statuses = Vec::with_capacity(requests.len());
    for request in requests {
        statuses.push(request.await??);
    }
    release.notify_one();
    holder.await?;
    let shed = statuses
        .iter()
        .filter(|status| **status == StatusCode::SERVICE_UNAVAILABLE)
        .count();
    assert_eq!(shed, 1, "{statuses:?}");

    drop((streams, app));
    runtime.shutdown().await
}

/// A stream past the 32 is answered 503 while the state lock is held, so
/// it never reached the runtime to subscribe. Paired with the stream that
/// opens once a slot is free. Fails with the slot taken after the
/// subscription.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_past_the_bound_is_refused_without_subscribing() -> Result<()> {
    let (runtime, identity, pod) = hosting_a_pod().await?;
    let app = router(Arc::clone(&runtime), true);
    let mut streams = open_streams(&app, identity, pod, MAX_OPEN_STREAMS).await?;

    let (release, holder) = hold_state_lock(&runtime).await?;
    // Denied (past the bound).
    let refused = tokio::time::timeout(PROMPT, open_events(&app, identity, pod))
        .await
        .context("the stream past the bound waited on the runtime")??;
    release.notify_one();
    holder.await?;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);

    streams.pop();
    let opened = open_events(&app, identity, pod).await?;
    assert_eq!(opened.status(), StatusCode::OK);

    drop((streams, opened, app));
    runtime.shutdown().await
}

/// An observed join past the 32 is answered 503 and joins nothing: the
/// joiner holds no store of the pod, and the invite it carried still admits
/// the observed join once a slot is free. Fails with the slot taken after
/// the join.
#[tokio::test(flavor = "multi_thread")]
async fn an_observed_join_past_the_bound_is_refused_without_joining() -> Result<()> {
    let inviter = Runtime::spawn(SpawnOptions::memory()).await?;
    let owner = inviter.identity().create().await?;
    let joined = inviter.pods().create(owner).await?;
    let invite = serde_json::to_vec(&inviter.pods().invite(owner, joined, None).await?)?;
    let (runtime, identity, pod) = hosting_a_pod().await?;
    let app = router(Arc::clone(&runtime), true);
    let mut streams = open_streams(&app, identity, pod, MAX_OPEN_STREAMS).await?;
    let join = || {
        Request::post(format!("/debug/identities/{identity}/pods/join-observed"))
            .body(Body::from(invite.clone()))
    };

    // Denied (past the bound).
    let refused = app.clone().oneshot(join()?).await?;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let held: Vec<PodId> = runtime
        .pods()
        .list(identity)
        .await?
        .into_iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(held, vec![pod], "a refused observed join joined");

    streams.pop();
    let answered = app.clone().oneshot(join()?).await?;
    assert_eq!(answered.status(), StatusCode::OK);
    assert_eq!(
        answered
            .headers()
            .get(POD_HEADER)
            .map(axum::http::HeaderValue::as_bytes),
        Some(joined.to_string().as_bytes())
    );

    drop((streams, answered, app));
    runtime.shutdown().await?;
    inviter.shutdown().await
}

/// The stop ends every open stream and the host's drain with it, well
/// inside the drain budget. Fails with the streams left to end with the
/// runtime.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_open_streams_within_the_drain() -> Result<()> {
    let (runtime, identity, pod) = hosting_a_pod().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move {
            serve(&runtime, listener, true, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    // Under iroh's `rustls-no-provider` the client takes only a process-wide
    // provider (`plain_http_tls` in `tests/common/mod.rs`).
    let _already = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::new();
    let mut streams = Vec::new();
    for _ in 0..2 {
        let stream = client
            .get(format!(
                "http://{address}/debug/identities/{identity}/pods/{pod}/events"
            ))
            .send()
            .await?;
        assert_eq!(stream.status().as_u16(), StatusCode::OK.as_u16());
        streams.push(stream);
    }

    let stopped_at = tokio::time::Instant::now();
    let _ = stop.send(());
    for stream in &mut streams {
        let end = tokio::time::timeout(GRACEFUL_DRAIN_BUDGET, async {
            while stream.chunk().await?.is_some() {}
            anyhow::Ok(())
        })
        .await
        .context("a stream outlived the drain")?;
        end?;
    }
    served.await??;
    assert!(
        stopped_at.elapsed() < GRACEFUL_DRAIN_BUDGET,
        "the drain ran out its budget: {:?}",
        stopped_at.elapsed()
    );

    runtime.shutdown().await
}
