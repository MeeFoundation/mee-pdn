//! Cells handlers, addressed by the identity performing the operation and
//! the cell. Record payloads and operations travel as raw bodies; a listing
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
    CellId, CellInvite, CellsService as _, PdnId, RecordId, RecordKind, RecordRef, Runtime,
};

use crate::{
    error::HostError,
    parse,
    shapes::{
        Act, HeldCell, HeldCells, Lifetime, Members, NoQuery, Operations, Placement, Records,
        UnknownEntries,
    },
};

/// `POST /debug/identities/{identity}/cells`.
pub(crate) async fn create(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<HeldCell>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let cell = runtime.cells().create(identity).await?;
    Ok(Json(HeldCell { cell }))
}

/// `GET /debug/identities/{identity}/cells`.
pub(crate) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<HeldCells>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let cells = runtime
        .cells()
        .list(identity)
        .await?
        .into_iter()
        .map(|info| info.id)
        .collect();
    Ok(Json(HeldCells { cells }))
}

/// `GET /debug/identities/{identity}/cells/{cell}/members`.
pub(crate) async fn members(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Members>, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let members = runtime
        .cells()
        .members(identity, cell)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(Members { members }))
}

/// `POST /debug/identities/{identity}/cells/{cell}/invites` — the payload
/// carries a live one-time secret.
pub(crate) async fn invite(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<CellInvite>, HostError> {
    let lifetime: Lifetime = parse::query(raw_query.as_deref(), "cell invite query")?;
    let (identity, cell) = addressed(&identity, &cell)?;
    let invite = runtime
        .cells()
        .invite(identity, cell, lifetime.as_duration()?)
        .await?;
    Ok(Json(invite))
}

/// `POST /debug/identities/{identity}/cells/join` — awaited to its end, the
/// catch-up included; the invite names the cell.
pub(crate) async fn join(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<Json<HeldCell>, HostError> {
    let identity = parse::segment(&identity, "identity")?;
    let invite: CellInvite = parse::json(&body, "cell invite")?;
    let cell = runtime.cells().join(identity, invite).await?;
    Ok(Json(HeldCell { cell }))
}

/// `POST /debug/identities/{identity}/cells/{cell}/acts`.
pub(crate) async fn act(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let act: Act = parse::json(&body, "cell act")?;
    runtime.cells().act(identity, cell, act.into()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /debug/identities/{identity}/cells/{cell}/records?kind=<kind>` —
/// placed under the identity's own name at a fresh id.
pub(crate) async fn put_record(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Result<Json<RecordRef>, HostError> {
    let placement: Placement = parse::query(raw_query.as_deref(), "record placement")?;
    let (identity, cell) = addressed(&identity, &cell)?;
    nonempty(&body, "a record")?;
    let record = runtime
        .cells()
        .put_record(identity, cell, placement.kind, &body)
        .await?;
    Ok(Json(record))
}

/// `GET /debug/identities/{identity}/cells/{cell}/records`.
pub(crate) async fn list_records(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Records>, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let records = runtime.cells().list_records(identity, cell).await?;
    Ok(Json(Records { records }))
}

/// `GET …/cells/{cell}/records/{member}/{kind}/{id}` — a claim or an
/// immutable-document; 404 covers both "no such record" and "payload still
/// arriving", as an entry read does.
pub(crate) async fn read(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Bytes, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let record = record(&member, &kind, &id)?;
    runtime
        .cells()
        .read(identity, cell, record)
        .await?
        .map(Bytes::from)
        .ok_or_else(|| {
            HostError::not_found(format!(
                "{} {} under {} reads on no entry here",
                record.kind, record.id, record.member
            ))
        })
}

/// `POST …/cells/{cell}/records/{member}/{kind}/{id}/ops`.
pub(crate) async fn append_op(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let record = record(&member, &kind, &id)?;
    nonempty(&body, "an operation")?;
    runtime
        .cells()
        .append_op(identity, cell, record, &body)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET …/cells/{cell}/records/{member}/{kind}/{id}/ops`.
pub(crate) async fn read_ops(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell, member, kind, id)): Path<(String, String, String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Operations>, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let record = record(&member, &kind, &id)?;
    let operations = runtime
        .cells()
        .read_ops(identity, cell, record)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(Operations { operations }))
}

/// `GET /debug/identities/{identity}/cells/{cell}/unknown`.
pub(crate) async fn list_unknown(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, cell)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<UnknownEntries>, HostError> {
    let (identity, cell) = addressed(&identity, &cell)?;
    let entries = runtime
        .cells()
        .list_unknown(identity, cell)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(UnknownEntries { entries }))
}

fn addressed(identity: &str, cell: &str) -> Result<(PdnId, CellId), HostError> {
    Ok((
        parse::segment(identity, "identity")?,
        parse::segment(cell, "cell")?,
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
