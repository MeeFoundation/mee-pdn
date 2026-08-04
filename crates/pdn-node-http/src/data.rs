//! Data handlers: entries of an issuer's data namespace, addressed by
//! issuer and path.
//!
//! The payload is the request body on the way in and the response body on
//! the way out — no base64, no JSON string escaping, so a replication
//! assertion tests replication and not an encoding both sides had to agree
//! on.
//!
//! Issuers, not hosted identities: a hosted identity's own namespace and a
//! namespace a grant brought in are both addressed the same way here, and
//! the second belongs to no hosted identity at all.

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
    shapes::{Entries, ListingPrefix},
};

/// `PUT /debug/data/{issuer}/{*path}` — the body's bytes become the entry.
///
/// Success is local: for a granted namespace the write passes the courtesy
/// check against the grant record this node has read, and the issuer's
/// ingest gate decides afterwards — a claim its own record does not cover
/// is refused there and the provisional entry is retracted here. The
/// retraction verdict does not cross this surface, so an answer here says
/// the write was admitted locally, not that the issuer kept it.
pub(crate) async fn write(
    State(runtime): State<Arc<Runtime>>,
    Path((issuer, path)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let issuer = parse::id(&issuer, "issuer")?;
    let path = parse::entry_path(&path)?;
    // The engine rejects a zero-length entry, and that rejection is the
    // request being wrong rather than the host not knowing what happened —
    // where an unnamed engine error would land.
    if body.is_empty() {
        return Err(HostError::bad_request(format!(
            "an entry payload is at least one byte: nothing to write at {path} under {issuer}"
        )));
    }
    runtime.data().write(issuer, &path, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /debug/data/{issuer}/{*path}` — the entry's bytes, or 404.
///
/// Absence covers both "no such entry" and "the record is here and its
/// payload is still arriving": the surface reports the present moment, and
/// the caller repeating the read is what waiting for convergence is.
pub(crate) async fn read(
    State(runtime): State<Arc<Runtime>>,
    Path((issuer, path)): Path<(String, String)>,
) -> Result<Bytes, HostError> {
    let issuer = parse::id(&issuer, "issuer")?;
    let path = parse::entry_path(&path)?;
    runtime
        .data()
        .read(issuer, &path)
        .await?
        .map(Bytes::from)
        .ok_or_else(|| HostError::not_found(format!("no entry at {path} under {issuer}")))
}

/// `GET /debug/data/{issuer}` — entry metadata, optionally narrowed by a
/// `prefix` query parameter matching whole path components.
pub(crate) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Path(issuer): Path<String>,
    Query(prefix): Query<ListingPrefix>,
) -> Result<Json<Entries>, HostError> {
    let issuer = parse::id(&issuer, "issuer")?;
    let prefix = prefix
        .prefix
        .as_deref()
        .map(parse::entry_path)
        .transpose()?;
    let entries = runtime.data().list(issuer, prefix.as_ref()).await?;
    Ok(Json(Entries { entries }))
}
