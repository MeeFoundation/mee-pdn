//! Connection handlers: the pairing ceremony's two halves, the connection
//! list, and the grant surface over a connection's metadata pair.
//!
//! Reads report what the runtime reports right now. A grant record whose
//! ticket payload is still arriving reads as no grant yet, and the caller
//! polls — the host inventing a wait would hide the difference between slow
//! and never, exactly where a harness needs to see it.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::StatusCode,
    Json,
};
use pdn_node::{ConnectionsService as _, InvitePayload, NonEmpty, Runtime};

use crate::{
    error::HostError,
    parse,
    shapes::{Connections, GrantPublication, Lifetime, NoQuery, PeerGrants},
};

/// `POST /debug/identities/{identity}/invite` — mint the payload a
/// counterparty consumes. Like a linking payload it carries a live one-time
/// secret, and a captured one can be consumed in the intended
/// counterparty's place.
pub(crate) async fn invite(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<InvitePayload>, HostError> {
    let lifetime: Lifetime = parse::query(raw_query.as_deref(), "invite query")?;
    let identity = parse::id(&identity, "identity")?;
    let payload = runtime
        .connections()
        .invite(identity, lifetime.as_duration()?)
        .await?;
    Ok(Json(payload))
}

/// `POST /debug/identities/{identity}/establish` — consume an invite
/// payload and run the establishment dialogue to its end.
pub(crate) async fn establish(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let invite: InvitePayload = parse::json(&body, "invite payload")?;
    runtime.connections().establish(identity, invite).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /debug/identities/{identity}/connections`.
pub(crate) async fn list(
    State(runtime): State<Arc<Runtime>>,
    Path(identity): Path<String>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<Connections>, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let connections = runtime.connections().list(identity).await?;
    Ok(Json(Connections { connections }))
}

/// `POST /debug/identities/{identity}/grants/{peer}` — publish a grant of
/// the granting identity's own data on exactly the claims named.
pub(crate) async fn publish_grant(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, peer)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let peer = parse::id(&peer, "peer")?;
    let publication: GrantPublication = parse::json(&body, "grant publication")?;
    let claims = NonEmpty::from_vec(publication.claims)
        .ok_or_else(|| HostError::bad_request("a grant names at least one claim"))?;
    runtime
        .connections()
        .publish_grant(identity, peer, publication.issuer, claims)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /debug/identities/{identity}/grants/{peer}` — the capabilities the
/// peer published toward this identity, without their tickets.
pub(crate) async fn read_grants(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, peer)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<PeerGrants>, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let peer = parse::id(&peer, "peer")?;
    let grants = runtime
        .connections()
        .read_grants(identity, peer)
        .await?
        .into_iter()
        .map(|peer_grant| peer_grant.grant.into())
        .collect();
    Ok(Json(PeerGrants { grants }))
}

/// `DELETE /debug/identities/{identity}/grants/{peer}/{issuer}` — withdraw
/// the grant of that issuer's data toward that peer.
pub(crate) async fn withdraw_grant(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, peer, issuer)): Path<(String, String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<StatusCode, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let peer = parse::id(&peer, "peer")?;
    let issuer = parse::id(&issuer, "issuer")?;
    runtime
        .connections()
        .withdraw_grant(identity, peer, issuer)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
