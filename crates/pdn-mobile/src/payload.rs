//! The textual form of a ceremony payload — what a screen draws as a code
//! and a scan rebuilds.
//!
//! Inside the text are the bytes the runtime's own payload type serializes
//! to, unwrapped: a payload's whole purpose is to be consumed by another
//! node, and that node may sit behind a different host over the same
//! runtime. A payload only this host could parse would leave a phone unable
//! to finish a ceremony with anything else, while still being called
//! "whatever the runtime minted".
//!
//! The facade names no field of a payload, inspects none, and decides
//! nothing from one. A code carrying an invitation to connect and a code
//! carrying a device joining an identity are told apart by the call the
//! caller makes, not by anything read here — which is why a code read for
//! the wrong act reaches the runtime and comes back as the runtime's
//! refusal.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{de::DeserializeOwned, Serialize};

use crate::error::PdnError;

/// A payload as text: its serde bytes in an alphabet that survives a code
/// and a camera.
pub(crate) fn encode<T: Serialize>(payload: &T) -> Result<String, PdnError> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => Ok(URL_SAFE_NO_PAD.encode(bytes)),
        Err(err) => {
            tracing::error!("a minted payload did not serialize: {err}");
            Err(PdnError::Internal)
        }
    }
}

/// A payload from the text a scan produced. A text that is not one is the
/// host's refusal; a payload of a version this runtime does not speak is
/// the runtime's, and is left to it.
pub(crate) fn decode<T: DeserializeOwned>(text: &str) -> Result<T, PdnError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|err| PdnError::malformed(format!("a code that carries no payload: {err}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| PdnError::malformed(format!("a code that carries no payload: {err}")))
}

#[cfg(test)]
mod tests {
    use pdn_node::InvitePayload;

    use super::*;

    /// A text that is not a payload of the kind the call expects is the
    /// host's own refusal. What a real payload does through this pair of
    /// functions — round-trip through a code, and interoperate with the
    /// bytes `pdn-node-http` takes — is asserted in `tests/surface.rs`,
    /// against a payload a running runtime minted rather than one composed
    /// here.
    #[test]
    fn a_text_that_carries_no_payload_is_the_hosts_refusal() {
        assert!(matches!(
            decode::<InvitePayload>("not a code").unwrap_err(),
            PdnError::MalformedInput { .. }
        ));
        assert!(matches!(
            decode::<InvitePayload>(&URL_SAFE_NO_PAD.encode(b"{}")).unwrap_err(),
            PdnError::MalformedInput { .. }
        ));
        assert!(matches!(
            decode::<InvitePayload>("").unwrap_err(),
            PdnError::MalformedInput { .. }
        ));
    }
}
