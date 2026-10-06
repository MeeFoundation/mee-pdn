//! Pods handlers, addressed by the identity performing the operation and
//! the pod. Record payloads and operations travel as raw bodies; a listing
//! of operations carries them in JSON. No route writes a raw entry or hands
//! over a store ticket: an invite carries a one-time secret, and the tickets
//! cross only inside the join dialogue.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::StatusCode,
    Json,
};
use pdn_node::{
    PdnId, PodId, PodInvite, PodsService as _, RecordId, RecordKind, RecordRef, Runtime,
};

use crate::{
    error::HostError,
    parse,
    shapes::{
        Act, HeldPod, HeldPods, Lifetime, Members, NoQuery, Operations, Placement, Records,
        UnknownEntries,
    },
};

/// `POST /debug/identities/{identity}/pods`.
pub(crate) async fn create(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<HeldPod>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let pod = runtime.pods().create(identity).await?;
    Ok(Json(HeldPod { pod }))
}

/// `GET /debug/identities/{identity}/pods`.
pub(crate) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<HeldPods>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let pods = runtime
        .pods()
        .list(identity)
        .await?
        .into_iter()
        .map(|info| info.id)
        .collect();
    Ok(Json(HeldPods { pods }))
}

/// `GET /debug/identities/{identity}/pods/{pod}/members`.
pub(crate) async fn members(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Members>, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let members = runtime
        .pods()
        .members(identity, pod)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(Members { members }))
}

/// `POST /debug/identities/{identity}/pods/{pod}/invites` — the payload
/// carries a live one-time secret.
pub(crate) async fn invite(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<PodInvite>, HostError> {
    let lifetime: Lifetime = parse::query(raw_query.as_deref(), "pod invite query")?;
    let (identity, pod) = addressed(&identity, &pod)?;
    let invite = runtime
        .pods()
        .invite(identity, pod, lifetime.as_duration()?)
        .await?;
    Ok(Json(invite))
}

/// `POST /debug/identities/{identity}/pods/join` — awaited to its end, the
/// catch-up included; the invite names the pod.
pub(crate) async fn join(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<Json<HeldPod>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let invite: PodInvite = parse::json(&body, "pod invite")?;
    let pod = runtime.pods().join(identity, invite).await?;
    Ok(Json(HeldPod { pod }))
}

/// `POST /debug/identities/{identity}/pods/{pod}/acts`.
pub(crate) async fn act(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let act: Act = parse::json(&body, "pod act")?;
    runtime.pods().act(identity, pod, act.into()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /debug/identities/{identity}/pods/{pod}/records?kind=<kind>` —
/// placed under the identity's own name at a fresh id.
pub(crate) async fn put_record(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Result<Json<RecordRef>, HostError> {
    let placement: Placement = parse::query(raw_query.as_deref(), "record placement")?;
    let (identity, pod) = addressed(&identity, &pod)?;
    nonempty(&body, "a record")?;
    let record = runtime
        .pods()
        .put_record(identity, pod, placement.kind, &body)
        .await?;
    Ok(Json(record))
}

/// `GET /debug/identities/{identity}/pods/{pod}/records`.
pub(crate) async fn list_records(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Records>, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let records = runtime.pods().list_records(identity, pod).await?;
    Ok(Json(Records { records }))
}

/// `GET …/pods/{pod}/records/{member}/{kind}/{id}` — a claim or an
/// immutable-document; 404 covers both "no such record" and "payload still
/// arriving", as an entry read does.
pub(crate) async fn read(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Bytes, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let record = record(&member, &kind, &id)?;
    runtime
        .pods()
        .read(identity, pod, record)
        .await?
        .map(Bytes::from)
        .ok_or_else(|| {
            HostError::not_found(format!(
                "{} {} under {} reads on no entry here",
                record.kind, record.id, record.member
            ))
        })
}

/// `POST …/pods/{pod}/records/{member}/{kind}/{id}/ops`.
pub(crate) async fn append_op(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let record = record(&member, &kind, &id)?;
    nonempty(&body, "an operation")?;
    runtime
        .pods()
        .append_op(identity, pod, record, &body)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET …/pods/{pod}/records/{member}/{kind}/{id}/ops`.
pub(crate) async fn read_ops(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Operations>, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let record = record(&member, &kind, &id)?;
    let operations = runtime
        .pods()
        .read_ops(identity, pod, record)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(Operations { operations }))
}

/// `GET /debug/identities/{identity}/pods/{pod}/unknown`.
pub(crate) async fn list_unknown(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, pod)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<UnknownEntries>, HostError> {
    let (identity, pod) = addressed(&identity, &pod)?;
    let entries = runtime
        .pods()
        .list_unknown(identity, pod)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(UnknownEntries { entries }))
}

fn addressed(identity: &str, pod: &str) -> Result<(PdnId, PodId), HostError> {
    Ok((
        parse::segment(identity, "identity")?,
        parse::segment(pod, "pod")?,
    ))
}

fn record(member: &str, kind: &str, id: &str) -> Result<RecordRef, HostError> {
    Ok(RecordRef {
        member: parse::segment(member, "member")?,
        kind: parse::segment::<RecordKind>(kind, "record kind")?,
        id: parse::segment::<RecordId>(id, "record id")?,
    })
}

/// The engine keeps no zero-length entry: the request being wrong, not the
/// 500 an unnamed engine error would land on.
fn nonempty(body: &Bytes, what: &str) -> Result<(), HostError> {
    if body.is_empty() {
        return Err(HostError::bad_request(format!(
            "{what} payload is at least one byte"
        )));
    }
    Ok(())
}
