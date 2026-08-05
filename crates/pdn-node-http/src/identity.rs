//! Identity handlers: create an identity, list the hosted ones, and the
//! device-linking ceremony's two halves.
//!
//! `link` runs a network dialogue and then waits for catch-up, so the
//! handler awaits it and answers when it is done — a stand whose callers are
//! a test and a shell script is happy to wait, and a job id would be host
//! state the runtime does not have.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use pdn_node::{IdentityService as _, LinkingPayload, Runtime, SyncService as _};

use crate::{
    error::HostError,
    parse,
    shapes::{CreatedIdentity, HostedIdentities, Lifetime, LinkBudget, NoQuery},
};

/// `POST /debug/identities` — an identity on its first device.
pub(crate) async fn create(
    State(runtime): State<Arc<Runtime>>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<CreatedIdentity>, HostError> {
    let identity = runtime.identity().create().await?;
    Ok(Json(CreatedIdentity { identity }))
}

/// `GET /debug/identities` — the identities this runtime hosts.
pub(crate) async fn hosted(
    State(runtime): State<Arc<Runtime>>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<HostedIdentities>, HostError> {
    let identities = runtime.sync().hosted_identities().await?;
    Ok(Json(HostedIdentities { identities }))
}

/// `POST /debug/identities/{identity}/linking-invite` — mint the payload a
/// new device consumes. It carries a live one-time secret: whoever captures
/// it can link in the intended device's place until it is burnt or expires.
pub(crate) async fn linking_invite(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(lifetime): Query<Lifetime>,
) -> Result<Json<LinkingPayload>, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let payload = runtime
        .identity()
        .linking_invite(identity, lifetime.as_duration()?)
        .await?;
    Ok(Json(payload))
}

/// `POST /debug/link` — consume a linking payload: dial, verify, import,
/// and return caught up. Addressed to no hosted identity, because the
/// payload names the one being joined.
pub(crate) async fn link(
    State(runtime): State<Arc<Runtime>>,
    Query(budget): Query<LinkBudget>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let payload: LinkingPayload = parse::json(&body, "linking payload")?;
    runtime
        .identity()
        .link(payload, budget.as_duration()?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
