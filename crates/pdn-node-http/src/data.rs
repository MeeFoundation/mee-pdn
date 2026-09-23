//! Data handlers, addressed by the identity performing the operation, the
//! issuer and the path. The payload is the raw request or response body,
//! so a replication assertion tests replication and not an encoding both
//! sides had to agree on.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use pdn_node::{DataService as _, Runtime};

use crate::{
    error::HostError,
    parse,
    shapes::{Entries, ListingPrefix, NoQuery},
};

/// `PUT /debug/data/{identity}/{issuer}/{*path}`. Success is local: the
/// issuer's ingest gate decides afterwards, and the retraction verdict
/// does not cross this surface.
pub(crate) async fn write(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, issuer, path)): Path<(String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let issuer = parse::id(&issuer, "issuer")?;
    let path = parse::entry_path(&path)?;
    // The engine rejects a zero-length entry: the request being wrong, not
    // the 500 an unnamed engine error would land on.
    if body.is_empty() {
        return Err(HostError::bad_request(format!(
            "an entry payload is at least one byte: nothing to write at {path} under {issuer}"
        )));
    }
    runtime.data().write(identity, issuer, &path, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /debug/data/{identity}/{issuer}/{*path}` — 404 covers both "no
/// such entry" and "payload still arriving"; repeating the read is the
/// wait.
pub(crate) async fn read(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, issuer, path)): Path<(String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Bytes, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let issuer = parse::id(&issuer, "issuer")?;
    let path = parse::entry_path(&path)?;
    runtime
        .data()
        .read(identity, issuer, &path)
        .await?
        .map(Bytes::from)
        .ok_or_else(|| HostError::not_found(format!("no entry at {path} under {issuer}")))
}

/// `GET /debug/data/{identity}/{issuer}` — `prefix` matches whole path
/// components.
pub(crate) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, issuer)): Path<(String, String)>,
    Query(prefix): Query<ListingPrefix>,
) -> Result<Json<Entries>, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let issuer = parse::id(&issuer, "issuer")?;
    let prefix = prefix
        .prefix
        .as_deref()
        .map(|value| value.trim_end_matches('/'))
        .filter(|value| !value.is_empty())
        .map(parse::entry_path)
        .transpose()?;
    let entries = runtime
        .data()
        .list(identity, issuer, prefix.as_ref())
        .await?;
    Ok(Json(Entries { entries }))
}
