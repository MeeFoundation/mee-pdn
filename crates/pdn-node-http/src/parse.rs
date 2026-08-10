//! Turning path segments and bodies into the runtime's types — the one
//! place a malformed request becomes 400.
//!
//! Bodies are decoded here rather than by axum's `Json` extractor, whose
//! rejection for a well-formed body of the wrong shape is 422: the error
//! table has one status for "the request itself is wrong", and a second one
//! arriving from the framework would be a distinction no caller asked for.

use axum::body::Bytes;
use pdn_node::{EntryPath, PdnId};
use serde::de::DeserializeOwned;

use crate::error::HostError;

/// A `PdnId` from a path segment — `what` names the segment in the refusal.
pub fn id(raw: &str, what: &str) -> Result<PdnId, HostError> {
    let preview: String = raw.chars().take(64).collect();
    raw.parse()
        .map_err(|err| HostError::bad_request(format!("malformed {what} {preview:?}: {err}")))
}

/// An `EntryPath` from the wildcard segment of a data route.
pub fn entry_path(raw: &str) -> Result<EntryPath, HostError> {
    let preview: String = raw.chars().take(64).collect();
    EntryPath::new(raw)
        .map_err(|err| HostError::bad_request(format!("malformed entry path {preview:?}: {err}")))
}

pub fn query<T: DeserializeOwned>(raw: Option<&str>, what: &str) -> Result<T, HostError> {
    serde_urlencoded::from_str(raw.unwrap_or_default())
        .map_err(|err| HostError::bad_request(format!("malformed {what}: {err}")))
}

/// A JSON request body.
pub fn json<T: DeserializeOwned>(body: &Bytes, what: &str) -> Result<T, HostError> {
    serde_json::from_slice(body)
        .map_err(|err| HostError::bad_request(format!("malformed {what}: {err}")))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn a_malformed_identity_is_400() {
        let err = id("not-hex", "identity").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_malformed_entry_path_is_400() {
        let err = entry_path("contact//email").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    /// A well-formed body of the wrong shape is 400 like any other
    /// malformed request, not axum's 422.
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
