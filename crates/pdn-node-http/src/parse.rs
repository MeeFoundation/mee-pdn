//! The one place a malformed request becomes 400. Bodies are decoded here
//! rather than by axum's `Json` extractor, whose rejection for a
//! well-formed body of the wrong shape is 422 — a distinction no caller
//! asked for.

use std::{fmt::Display, str::FromStr};

use axum::body::Bytes;
use pdn_node::EntryPath;
use serde::de::DeserializeOwned;

use crate::error::HostError;

/// A path segment by its text form — an identity, a pod, a record id or
/// kind; `what` names the segment in the refusal.
pub fn segment<T>(raw: &str, what: &str) -> Result<T, HostError>
where
    T: FromStr,
    T::Err: Display,
{
    let preview: String = raw.chars().take(64).collect();
    raw.parse()
        .map_err(|err| HostError::bad_request(format!("malformed {what} {preview:?}: {err}")))
}

pub fn entry_path(raw: &str) -> Result<EntryPath, HostError> {
    let preview: String = raw.chars().take(64).collect();
    EntryPath::new(raw)
        .map_err(|err| HostError::bad_request(format!("malformed entry path {preview:?}: {err}")))
}

pub fn query<T: DeserializeOwned>(raw: Option<&str>, what: &str) -> Result<T, HostError> {
    serde_urlencoded::from_str(raw.unwrap_or_default())
        .map_err(|err| HostError::bad_request(format!("malformed {what}: {err}")))
}

pub fn json<T: DeserializeOwned>(body: &Bytes, what: &str) -> Result<T, HostError> {
    serde_json::from_slice(body)
        .map_err(|err| HostError::bad_request(format!("malformed {what}: {err}")))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use pdn_node::{PdnId, PodId, RecordKind};

    use super::*;

    #[test]
    fn a_malformed_identity_is_400() {
        let err = segment::<PdnId>("not-hex", "identity").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    /// An identity's 64 hex chars are no pod id.
    #[test]
    fn a_malformed_pod_is_400() {
        let identity = PdnId::from_bytes([0x11; 32]).to_string();
        let err = segment::<PodId>(&identity, "pod").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn an_unknown_record_kind_is_400() {
        let err = segment::<RecordKind>("Claim", "record kind").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_malformed_entry_path_is_400() {
        let err = entry_path("contact//email").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_body_of_the_wrong_shape_is_400() {
        let err = json::<PdnId>(&Bytes::from_static(b"{\"a\":1}"), "an identity").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_malformed_query_is_400() {
        let err = query::<crate::shapes::Lifetime>(Some("lifetime_secs=nope"), "invite query")
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
