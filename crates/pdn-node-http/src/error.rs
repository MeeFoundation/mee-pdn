//! One closed table from a runtime error to a status — what the stand's
//! deny tests rest on: a refusal must be distinguishable from an absent
//! route and from a host that does not understand what happened. An
//! unmapped error is 500, the pessimistic reading: a clean refusal would
//! launder a host bug into a verdict. The router's body ceiling answers
//! axum's own 413 before any handler runs, outside this table.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use pdn_node::{
    ActRefused, DelegationUnsupported, EstablishmentInProgress, EstablishmentRefused,
    IdentityAlreadyHosted, IdentityKeyPending, JoinInProgress, JoinRefused, LinkingInProgress,
    LinkingRefused, PeerNotConnected, RecordPlacedOnce, UnknownIdentity, UnknownIssuer, UnknownPod,
    UnknownRecord, UnsupportedInviteVersion, UnsupportedLinkingVersion,
    UnsupportedPodInviteVersion, WriteNotGranted, WrongRecordKind,
};

#[derive(Debug)]
pub struct HostError {
    status: StatusCode,
    message: String,
}

impl HostError {
    /// The request itself is wrong.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// An absent entry or record alone. An identity, issuer or pod the
    /// runtime does not know is 409, never this: route names are unpinned, so
    /// a deny test asserting 404 would keep passing after a rename.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

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

/// Downcasting reaches through `anyhow`'s context layers.
fn status_of(err: &anyhow::Error) -> StatusCode {
    if err.downcast_ref::<EstablishmentRefused>().is_some()
        || err.downcast_ref::<LinkingRefused>().is_some()
        || err.downcast_ref::<JoinRefused>().is_some()
        || err.downcast_ref::<WriteNotGranted>().is_some()
        || err.downcast_ref::<DelegationUnsupported>().is_some()
        || err.downcast_ref::<ActRefused>().is_some()
        || err.downcast_ref::<RecordPlacedOnce>().is_some()
    {
        // The runtime's rules said no, or a ceremony reached its inviter and
        // no answer came back.
        StatusCode::FORBIDDEN
    } else if err.downcast_ref::<UnknownIdentity>().is_some()
        || err.downcast_ref::<UnknownIssuer>().is_some()
        || err.downcast_ref::<UnknownPod>().is_some()
        || err.downcast_ref::<PeerNotConnected>().is_some()
        || err.downcast_ref::<IdentityKeyPending>().is_some()
        || err.downcast_ref::<IdentityAlreadyHosted>().is_some()
        || err.downcast_ref::<LinkingInProgress>().is_some()
        || err.downcast_ref::<EstablishmentInProgress>().is_some()
        || err.downcast_ref::<JoinInProgress>().is_some()
    {
        // The runtime does not host what the request addressed, or already
        // has a conflicting act of its own committed or in flight against
        // it. A pod the identity is no member of is here, not with the
        // refusals: a test expecting a refusal by role would otherwise pass
        // when the service does not take the caller for a member at all.
        StatusCode::CONFLICT
    } else if err.downcast_ref::<UnsupportedInviteVersion>().is_some()
        || err.downcast_ref::<UnsupportedLinkingVersion>().is_some()
        || err.downcast_ref::<UnsupportedPodInviteVersion>().is_some()
        || err.downcast_ref::<WrongRecordKind>().is_some()
    {
        StatusCode::BAD_REQUEST
    } else if err.downcast_ref::<UnknownRecord>().is_some() {
        StatusCode::NOT_FOUND
    } else {
        // An unreachable peer included: 500 against 403 is the distinction.
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Context as _};
    use pdn_node::{ActRefusal, EntryPath, PdnId, PodAct, PodId, RecordId, RecordKind, RecordRef};

    use super::*;

    const ISSUER: PdnId = PdnId::from_bytes([0x11; 32]);
    const PEER: PdnId = PdnId::from_bytes([0x22; 32]);
    const POD: PodId = PodId::from_bytes([0x33; 16]);

    fn record(kind: RecordKind) -> RecordRef {
        RecordRef {
            member: PEER,
            kind,
            id: RecordId::from_bytes([0x44; 16]),
        }
    }

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
        assert_eq!(status(JoinRefused), StatusCode::FORBIDDEN);
        assert_eq!(
            status(ActRefused {
                act: PodAct::Remove(PEER),
                reason: ActRefusal::NotAnOwner,
            }),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(RecordPlacedOnce {
                record: record(RecordKind::Claim),
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
        assert_eq!(status(UnknownPod { pod: POD }), StatusCode::CONFLICT);
        assert_eq!(
            status(IdentityKeyPending { identity: ISSUER }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(JoinInProgress {
                identity: ISSUER,
                pod: POD,
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
        assert_eq!(
            status(UnsupportedPodInviteVersion { version: 9 }),
            StatusCode::BAD_REQUEST
        );
    }

    /// The kind in the path names a call that does not read it.
    #[test]
    fn a_record_read_by_the_wrong_call_is_400() {
        assert_eq!(
            status(WrongRecordKind {
                record: record(RecordKind::MergeableDocument),
            }),
            StatusCode::BAD_REQUEST
        );
    }

    /// A record that reads on no entry here is absent, as an entry is.
    #[test]
    fn an_absent_record_is_404() {
        assert_eq!(
            status(UnknownRecord {
                record: record(RecordKind::MergeableDocument),
            }),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn an_unmapped_error_is_500() {
        let err = HostError::from(anyhow!("the inviter was never reached"));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message, "internal server error");
    }

    #[test]
    fn a_wrapped_refusal_keeps_its_status() {
        let wrapped = Err::<(), _>(anyhow::Error::new(LinkingRefused))
            .context("linking into the identity")
            .unwrap_err();
        assert_eq!(HostError::from(wrapped).status(), StatusCode::FORBIDDEN);
    }

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
