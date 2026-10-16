//! The observation routes (`observation` feature, the host spec's
//! observation requirement): one route to each of the runtime's test-only
//! observation calls. An event stream holds one of [`MAX_OPEN_STREAMS`]
//! slots, none of the request permits once its headers are sent, and ends
//! with the runtime or at the host's stop.

use std::{convert::Infallible, sync::Arc};

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse as _, Response},
    routing::{get, post},
    Json, Router,
};
use pdn_node::{
    observation::{ListedEntry, PodSubscription},
    PodId, PodInvite, Runtime,
};
use serde::Deserialize;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use crate::{error::HostError, parse, shapes::NoQuery, POD_HEADER};

/// Event streams open at once; past it, 503 without subscribing.
pub const MAX_OPEN_STREAMS: usize = 32;

#[derive(Clone)]
struct Observation {
    runtime: Arc<Runtime>,
    streams: Arc<Semaphore>,
    stopping: watch::Receiver<bool>,
}

pub(crate) fn routes(
    runtime: Arc<Runtime>,
    stopping: watch::Receiver<bool>,
) -> Router<Arc<Runtime>> {
    Router::new()
        .route(
            "/debug/identities/{identity}/pods/{pod}/events",
            get(events),
        )
        .route(
            "/debug/identities/{identity}/pods/join-observed",
            post(join_observed),
        )
        .route(
            "/debug/identities/{identity}/pods/{pod}/listing",
            get(listing),
        )
        .with_state(Observation {
            runtime,
            streams: Arc::new(Semaphore::new(MAX_OPEN_STREAMS)),
            stopping,
        })
}

/// `GET /debug/identities/{identity}/pods/{pod}/events`.
async fn events(
    State(observation): State<Observation>,
    Path((identity, pod)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Response, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let pod: PodId = parse::segment(&pod, "pod")?;
    let Ok(slot) = Arc::clone(&observation.streams).try_acquire_owned() else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let subscription = observation
        .runtime
        .observe_pod_for_test(identity, pod)
        .await?;
    Ok(record_lines(subscription, slot, observation.stopping).into_response())
}

/// `POST /debug/identities/{identity}/pods/join-observed` — the join
/// route's invite; its slot is taken before the join starts.
async fn join_observed(
    State(observation): State<Observation>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<Response, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let invite: PodInvite = parse::json(&body, "pod invite")?;
    let Ok(slot) = Arc::clone(&observation.streams).try_acquire_owned() else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let (pod, subscription) = observation
        .runtime
        .join_pod_observed_for_test(identity, invite)
        .await?;
    let mut response = record_lines(subscription, slot, observation.stopping).into_response();
    let pod = HeaderValue::from_str(&pod.to_string())
        .map_err(|err| HostError::from(anyhow::Error::new(err)))?;
    response.headers_mut().insert(POD_HEADER, pod);
    Ok(response)
}

/// `?payloads=false` lists without the payloads: a phone's listing of
/// large records would otherwise hold every payload in memory at once.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListingQuery {
    payloads: Option<bool>,
}

/// `GET /debug/identities/{identity}/pods/{pod}/listing`.
async fn listing(
    State(observation): State<Observation>,
    Path((identity, pod)): Path<(String, String)>,
    Query(ListingQuery { payloads }): Query<ListingQuery>,
) -> Result<Json<Vec<ListedEntry>>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let pod = parse::segment(&pod, "pod")?;
    Ok(Json(
        observation
            .runtime
            .pod_record_listing_for_test(identity, pod, payloads.unwrap_or(true))
            .await?,
    ))
}

/// One JSON record per line as the subscription yields it; `slot` is held
/// until the body ends.
fn record_lines(
    subscription: PodSubscription,
    slot: OwnedSemaphorePermit,
    stopping: watch::Receiver<bool>,
) -> ([(header::HeaderName, &'static str); 1], Body) {
    let lines = futures_lite::stream::unfold(
        (subscription, slot, stopping),
        |(mut subscription, slot, mut stopping)| async move {
            let record = tokio::select! {
                record = subscription.next() => record?,
                () = stopped(&mut stopping) => return None,
            };
            let mut line = serde_json::to_vec(&record).ok()?;
            line.push(b'\n');
            Some((
                Ok::<_, Infallible>(Bytes::from(line)),
                (subscription, slot, stopping),
            ))
        },
    );
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(lines),
    )
}

/// Pending forever once the sender is gone without a stop: a router built
/// with no stop to wait for.
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    if stopping.wait_for(|stopped| *stopped).await.is_err() {
        std::future::pending::<()>().await;
    }
}
