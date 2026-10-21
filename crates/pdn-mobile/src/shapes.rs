//! What crosses the binding boundary, and the parsing that decides a
//! malformed input before the runtime is called.
//!
//! Identities, pod ids and record ids cross as text: a screen shows them
//! and a person compares them by eye. Record payloads and operations cross
//! as bytes with no framing of the facade's own — what was written is what
//! is read.

use std::time::Duration;

use pdn_node::{Operation, PdnId, PodAct, PodId, PodMember, RecordId, RecordKind, RecordRef};

use crate::error::PdnError;

/// The widest duration a caller may name for a lifetime or a budget.
/// Generous, and far below the point where adding it to the runtime's
/// current instant would overflow.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

/// The ceiling on one record payload or operation. A payload crosses the boundary as one
/// buffer in memory, and a phone is killed for memory rather than asked to
/// swap, so an unbounded one ends the process instead of returning an
/// error. `pdn-node-http` bounds its own at 64 MB for a container; a
/// device gets a tighter one.
///
/// It bounds what this host puts in and not what a read hands back: a
/// record another member wrote — through a host whose own ceiling is 64
/// times this one — is in the replica whatever its size, and refusing to
/// hand it over would make the record unreadable rather than making the
/// device safer.
pub const MAX_ENTRY_PAYLOAD: u64 = 1_048_576;

/// A pod member and whether it is an owner.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PodMemberInfo {
    pub identity: String,
    pub owner: bool,
}

impl From<PodMember> for PodMemberInfo {
    fn from(member: PodMember) -> Self {
        Self {
            identity: member.id.to_string(),
            owner: member.owner,
        }
    }
}

/// The kind a record is placed as, chosen once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PodRecordKind {
    Claim,
    MergeableDocument,
    ImmutableDocument,
}

impl From<PodRecordKind> for RecordKind {
    fn from(kind: PodRecordKind) -> Self {
        match kind {
            PodRecordKind::Claim => Self::Claim,
            PodRecordKind::MergeableDocument => Self::MergeableDocument,
            PodRecordKind::ImmutableDocument => Self::ImmutableDocument,
        }
    }
}

impl From<RecordKind> for PodRecordKind {
    fn from(kind: RecordKind) -> Self {
        match kind {
            RecordKind::Claim => Self::Claim,
            RecordKind::MergeableDocument => Self::MergeableDocument,
            RecordKind::ImmutableDocument => Self::ImmutableDocument,
        }
    }
}

/// A record's address: the member it sits under, its kind and its id.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PodRecord {
    pub member: String,
    pub kind: PodRecordKind,
    pub id: String,
}

impl From<RecordRef> for PodRecord {
    fn from(record: RecordRef) -> Self {
        Self {
            member: record.member.to_string(),
            kind: record.kind.into(),
            id: record.id.to_string(),
        }
    }
}

/// A mergeable-document's operation, with its writer; merged into nothing.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PodOperation {
    pub writer: String,
    /// The operation's key segment, unique within the record.
    pub id: String,
    pub payload: Vec<u8>,
}

impl From<Operation> for PodOperation {
    fn from(operation: Operation) -> Self {
        Self {
            writer: operation.id.writer.to_string(),
            id: operation.id.to_string(),
            payload: operation.payload,
        }
    }
}

/// A membership act. Founding is `create`'s and inviting is the join's.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PodMembershipAct {
    Promote {
        member: String,
    },
    /// Of another owner.
    Demote {
        member: String,
    },
    /// Of another member, an owner or a plain member alike.
    Remove {
        member: String,
    },
    Leave,
}

impl PodMembershipAct {
    pub(crate) fn parse(&self) -> Result<PodAct, PdnError> {
        Ok(match self {
            Self::Promote { member } => PodAct::Promote(identity(member, "member")?),
            Self::Demote { member } => PodAct::Demote(identity(member, "member")?),
            Self::Remove { member } => PodAct::Remove(identity(member, "member")?),
            Self::Leave => PodAct::Leave,
        })
    }
}

/// A pod id from the text a screen holds.
pub(crate) fn pod(raw: &str) -> Result<PodId, PdnError> {
    let preview: String = raw.chars().take(64).collect();
    raw.parse()
        .map_err(|err| PdnError::malformed(format!("malformed pod {preview:?}: {err}")))
}

/// A record's address from the text a screen holds.
pub(crate) fn record(raw: &PodRecord) -> Result<RecordRef, PdnError> {
    let preview: String = raw.id.chars().take(64).collect();
    let id: RecordId = raw
        .id
        .parse()
        .map_err(|err| PdnError::malformed(format!("malformed record id {preview:?}: {err}")))?;
    Ok(RecordRef {
        member: identity(&raw.member, "member")?,
        kind: raw.kind.into(),
        id,
    })
}

/// An identity from the text a screen holds.
pub(crate) fn identity(raw: &str, what: &str) -> Result<PdnId, PdnError> {
    let preview: String = raw.chars().take(64).collect();
    raw.parse()
        .map_err(|err| PdnError::malformed(format!("malformed {what} {preview:?}: {err}")))
}

/// A caller's duration, refusing the two values that would otherwise
/// produce a confusing no-op or a panic downstream instead of a clean
/// refusal: zero, and anything past [`MAX_DURATION_SECS`].
pub(crate) fn duration(secs: u64, what: &str) -> Result<Duration, PdnError> {
    if secs == 0 {
        return Err(PdnError::malformed(format!("a {what} of 0 seconds")));
    }
    if secs > MAX_DURATION_SECS {
        return Err(PdnError::malformed(format!(
            "a {what} must be at most {MAX_DURATION_SECS} seconds, got {secs}"
        )));
    }
    Ok(Duration::from_secs(secs))
}

/// An optional lifetime: absent leaves the runtime's own default.
pub(crate) fn lifetime(secs: Option<u64>) -> Result<Option<Duration>, PdnError> {
    secs.map(|secs| duration(secs, "lifetime")).transpose()
}

/// A record payload or operation on its way in: bytes, unchanged, bounded.
pub(crate) fn entry_payload(payload: &[u8]) -> Result<(), PdnError> {
    if payload.is_empty() {
        return Err(PdnError::malformed(
            "an empty entry payload, which the engine keeps no entry for",
        ));
    }
    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    if len > MAX_ENTRY_PAYLOAD {
        return Err(PdnError::malformed(format!(
            "an entry payload of {len} bytes, above the ceiling of {MAX_ENTRY_PAYLOAD}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An identity that is no hex id is refused by the host itself as
    /// malformed input, before the runtime is called.
    #[test]
    fn a_malformed_identity_is_the_hosts_refusal() {
        let err = identity("not-hex", "identity").unwrap_err();
        assert!(matches!(err, PdnError::MalformedInput { .. }));
    }

    /// The boundaries `duration` exists for: `0` and one past the ceiling
    /// are refusals here rather than a panic or an already-expired invite
    /// deeper in.
    #[test]
    fn a_duration_outside_its_range_is_refused() {
        assert!(duration(0, "lifetime").is_err());
        assert!(duration(MAX_DURATION_SECS, "lifetime").is_ok());
        assert!(duration(MAX_DURATION_SECS + 1, "lifetime").is_err());
        assert!(lifetime(None).unwrap().is_none());
    }

    /// An empty payload and one a byte past `MAX_ENTRY_PAYLOAD` are refused;
    /// one byte is taken.
    #[test]
    fn an_empty_payload_and_one_above_the_ceiling_are_refused() {
        assert!(entry_payload(b"").is_err());
        assert!(entry_payload(b"a").is_ok());
        let too_big = vec![0_u8; usize::try_from(MAX_ENTRY_PAYLOAD).unwrap_or(usize::MAX) + 1];
        assert!(entry_payload(&too_big).is_err());
    }
}
