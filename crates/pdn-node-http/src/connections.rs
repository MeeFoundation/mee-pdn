//! Connection handlers. Reads report what the runtime reports right now:
//! a host inventing a wait would hide the difference between slow and
//! never, exactly where a harness needs to see it.

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
    shapes::{
        Connections, GrantCapability, GrantPublication, Lifetime, NoQuery, OwnGrant, PeerGrants,
    },
};

/// `POST /debug/identities/{identity}/invite` — the payload carries a live
/// one-time secret.
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

/// `POST /debug/identities/{identity}/establish`.
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

/// `POST /debug/identities/{identity}/grants/{peer}`.
pub(crate) async fn publish_grant(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, peer)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
    body: Bytes,
) -> Result<StatusCode, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let peer = parse::id(&peer, "peer")?;
    let publication: GrantPublication = parse::json(&body, "grant publication")?;
    let named = publication
        .claims
        .into_iter()
        .map(|granted| {
            let path = parse::entry_path(&granted.path)?;
            Ok(pdn_node::GrantedClaim {
                claim: pdn_node::claim_id_of(&publication.issuer, &path),
                write: granted.write,
            })
        })
        .collect::<Result<Vec<_>, HostError>>()?;
    let claims = NonEmpty::from_vec(named)
        .ok_or_else(|| HostError::bad_request("a grant names at least one claim"))?;
    runtime
        .connections()
        .publish_grant(identity, peer, publication.issuer, claims)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /debug/identities/{identity}/grants/{peer}` — capabilities without
/// their tickets.
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

/// `GET /debug/identities/{identity}/own-grants/{peer}` — as the answering
/// device holds it, without its ticket.
pub(crate) async fn read_own_grants(
    State(runtime): State<Arc<Runtime>>,
    Path((identity, peer)): Path<(String, String)>,
    Query(NoQuery {}): Query<NoQuery>,
) -> Result<Json<OwnGrant>, HostError> {
    let identity = parse::id(&identity, "identity")?;
    let peer = parse::id(&peer, "peer")?;
    let grant = runtime
        .connections()
        .read_own_grants(identity, peer)
        .await?
        .map(GrantCapability::from);
    Ok(Json(OwnGrant { grant }))
}

/// `DELETE /debug/identities/{identity}/grants/{peer}/{issuer}`.
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
