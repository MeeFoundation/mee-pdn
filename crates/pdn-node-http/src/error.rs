//! The host's error mapping: one closed table from a runtime error to a
//! status, and the response it becomes.
//!
//! The table is what the container stand's deny tests rest on. A refused
//! operation must never read as success, and it must be distinguishable
//! both from a route the host does not serve and from a host that does not
//! understand what happened — a surface that answers alike for "the
//! runtime refused you" and "you asked wrong" makes every paired denial
//! vacuous. An unmapped error is therefore 500, the pessimistic reading:
//! reporting it as a clean refusal would launder a host bug into a verdict.
//!
//! One request-shape limit sits outside this table by construction: the
//! router's request-body ceiling (64 MB — comfortably above a realistic
//! demo entry, far short of `pdn-store`'s own 1 GB wire ceiling) answers
//! axum's own 413 before a handler, and so before `HostError`, ever runs —
//! the same footing as the two statuses this type decides on its own
//! (`bad_request`, `not_found`).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use pdn_node::{
    DelegationUnsupported, EstablishmentInProgress, EstablishmentRefused, IdentityAlreadyHosted,
    LinkingInProgress, LinkingRefused, PeerNotConnected, UnknownIdentity, UnknownIssuer,
    UnsupportedInviteVersion, UnsupportedLinkingVersion, WriteNotGranted,
};

/// A failed operation on its way back to the caller: the status the table
/// assigned it, and the text of whatever produced it.
#[derive(Debug)]
pub struct HostError {
    status: StatusCode,
    message: String,
}

impl HostError {
    /// The request itself is wrong — an unparseable identifier, an
    /// undecodable body, a claim set with nothing in it.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// Nothing is there. The host's own 404s are for an absent entry alone;
    /// the router answers the same for a route it does not serve. An
    /// identity or issuer the runtime does not know is a refusal (409),
    /// never this — route names are unpinned, so a deny test asserting 404
    /// would keep passing after a rename.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// The status this error answers with.
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl From<anyhow::Error> for HostError {
    fn from(err: anyhow::Error) -> Self {
        let status = status_of(&err);
        let internal = format!("{err:#}");
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            "internal server error".to_owned()
        } else {
            err.to_string()
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("unmapped host error: {internal}");
        }
        Self { status, message }
    }
}

impl IntoResponse for HostError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

/// The closed table. Downcasting reaches through `anyhow`'s context layers,
/// so a service error wrapped on its way out still maps.
fn status_of(err: &anyhow::Error) -> StatusCode {
    if err.downcast_ref::<EstablishmentRefused>().is_some()
        || err.downcast_ref::<LinkingRefused>().is_some()
        || err.downcast_ref::<WriteNotGranted>().is_some()
        || err.downcast_ref::<DelegationUnsupported>().is_some()
    {
        // The runtime's rules said no.
        StatusCode::FORBIDDEN
    } else if err.downcast_ref::<UnknownIdentity>().is_some()
        || err.downcast_ref::<UnknownIssuer>().is_some()
        || err.downcast_ref::<PeerNotConnected>().is_some()
        || err.downcast_ref::<IdentityAlreadyHosted>().is_some()
        || err.downcast_ref::<LinkingInProgress>().is_some()
        || err.downcast_ref::<EstablishmentInProgress>().is_some()
    {
        // The runtime does not host what the request addressed, or already
        // has a conflicting act of its own committed or in flight against
        // it.
        StatusCode::CONFLICT
    } else if err.downcast_ref::<UnsupportedInviteVersion>().is_some()
        || err.downcast_ref::<UnsupportedLinkingVersion>().is_some()
    {
        StatusCode::BAD_REQUEST
    } else {
        // Including an unreachable peer, which needs no type of its own:
        // 500 against 403 is already the distinction a deny test rests on.
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Context as _};
    use pdn_node::{EntryPath, PdnId};

    use super::*;

    const ISSUER: PdnId = PdnId::from_bytes([0x11; 32]);
    const PEER: PdnId = PdnId::from_bytes([0x22; 32]);

    /// The status the table gives one error value.
    fn status(err: impl Into<anyhow::Error>) -> StatusCode {
        HostError::from(err.into()).status()
    }

    #[test]
    fn refusals_are_403() {
        assert_eq!(status(EstablishmentRefused), StatusCode::FORBIDDEN);
        assert_eq!(status(LinkingRefused), StatusCode::FORBIDDEN);
        assert_eq!(
            status(WriteNotGranted {
                issuer: ISSUER,
                path: EntryPath::new("contact/email").unwrap(),
            }),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(DelegationUnsupported {
                identity: PEER,
                issuer: ISSUER,
            }),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn what_the_runtime_does_not_host_is_409() {
        assert_eq!(
            status(UnknownIdentity { identity: PEER }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(UnknownIssuer { issuer: ISSUER }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(PeerNotConnected {
                identity: ISSUER,
                peer: PEER,
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(IdentityAlreadyHosted { identity: ISSUER }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(LinkingInProgress { identity: ISSUER }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(EstablishmentInProgress {
                identity: ISSUER,
                peer: PEER,
            }),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn unspoken_payload_versions_are_400() {
        assert_eq!(
            status(UnsupportedInviteVersion { version: 9 }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(UnsupportedLinkingVersion { version: 9 }),
            StatusCode::BAD_REQUEST
        );
    }

    /// The pessimistic default: what the host cannot name it does not
    /// launder into a refusal.
    #[test]
    fn an_unmapped_error_is_500() {
        let err = HostError::from(anyhow!("the inviter was never reached"));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message, "internal server error");
    }

    /// Service errors arrive wrapped in context; the table still reads
    /// through it.
    #[test]
    fn a_wrapped_refusal_keeps_its_status() {
        let wrapped = Err::<(), _>(anyhow::Error::new(LinkingRefused))
            .context("linking into the identity")
            .unwrap_err();
        assert_eq!(HostError::from(wrapped).status(), StatusCode::FORBIDDEN);
    }

    /// The error's own text travels with it, so a deny test can name what
    /// refused.
    #[test]
    fn the_response_carries_the_errors_text() {
        let err = HostError::from(anyhow::Error::new(UnknownIdentity { identity: PEER }));
        assert!(
            err.message.contains(&PEER.to_string()),
            "the message must name the identity: {}",
            err.message
        );
    }

    #[test]
    fn an_internal_cause_chain_never_reaches_the_response() {
        let err = anyhow!("private storage path /secret/node.db")
            .context("failed to open the identity directory");
        let host = HostError::from(err);
        assert_eq!(host.message, "internal server error");
        assert!(!host.message.contains("/secret/node.db"));
    }

    /// The two statuses the host decides on its own, not by downcast.
    #[test]
    fn the_hosts_own_statuses() {
        assert_eq!(
            HostError::bad_request("malformed").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            HostError::not_found("no entry").status(),
            StatusCode::NOT_FOUND
        );
    }
}
