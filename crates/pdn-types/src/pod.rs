//! The pod's vocabulary; the pod stores spec (`data-layer/pod-store`) holds its rules.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PdnId;

crate::define_byte_id_16! {
    /// A pod's one address, carrying no key material and never equal to
    /// either of its stores' namespace ids (Invariant 3).
    pub struct PodId;
}

crate::define_byte_id_16! {
    /// A record's id, 16 random bytes minted when the record is placed.
    pub struct RecordId;
}

/// The kind a record is placed as, chosen once; its name is the kind's
/// segment in the record's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordKind {
    Claim,
    MergeableDocument,
    ImmutableDocument,
}

impl RecordKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::MergeableDocument => "mergeable-document",
            Self::ImmutableDocument => "immutable-document",
        }
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A name that is none of the three kinds; matched exactly, case included.
#[derive(Debug, Clone, Error)]
#[error("unknown record kind: {0}")]
pub struct UnknownRecordKind(pub String);

impl FromStr for RecordKind {
    type Err = UnknownRecordKind;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [
            Self::Claim,
            Self::MergeableDocument,
            Self::ImmutableDocument,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == s)
        .ok_or_else(|| UnknownRecordKind(s.to_owned()))
    }
}

/// A record's identity, whatever its kind: the member under whose name it
/// sits, its kind and its id — its key without the last segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecordRef {
    pub member: PdnId,
    pub kind: RecordKind,
    pub id: RecordId,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each record kind parses back from its name, serde writes the same
    /// name, and a name differing in case is refused.
    #[test]
    fn record_kind_names_round_trip() {
        for (kind, name) in [
            (RecordKind::Claim, "claim"),
            (RecordKind::MergeableDocument, "mergeable-document"),
            (RecordKind::ImmutableDocument, "immutable-document"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(name.parse::<RecordKind>().unwrap(), kind);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{name}\""));
        }
        assert!("Claim".parse::<RecordKind>().is_err());
    }

    /// A pod id's text is 32 lowercase hex chars and parses back; a 64-char
    /// string is refused.
    #[test]
    fn pod_id_text_round_trips() {
        let id: PodId = "ad58a3faa04cdc5576c8dc5823a347c6".parse().unwrap();
        assert_eq!(id.to_string(), "ad58a3faa04cdc5576c8dc5823a347c6");
        assert!(
            "ad58a3faa04cdc5576c8dc5823a347c6ad58a3faa04cdc5576c8dc5823a347c6"
                .parse::<PodId>()
                .is_err()
        );
    }
}
