//! What crosses the binding boundary, and the parsing that decides a
//! malformed input before the runtime is called.
//!
//! Identities, pod ids and record ids cross as text: a screen shows them
//! and a person compares them by eye. Record payloads and operations cross
//! as bytes with no framing of the facade's own — what was written is what
//! is read.

use std::{fmt::Display, time::Duration};

use pdn_node::{Operation, PdnId, PodAct, PodId, PodMember, RecordId, RecordKind, RecordRef};

use crate::error::PdnError;

/// The widest duration a caller may name for a lifetime or a budget.
/// Generous, and far below the point where adding it to the runtime's
/// current instant would overflow.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

/// The ceiling on one claim's payload and on one operation of a mergeable-document, the first
/// included: every read of a document reads all its operations again. It bounds what this host
/// puts in, not what a read hands back (`pdn-mobile/host` spec).
pub const MAX_ENTRY_PAYLOAD: u64 = 1_048_576;

/// The ceiling on one immutable-document's payload — a file, read only when asked for.
pub const MAX_DOCUMENT_PAYLOAD: u64 = 104_857_600;

/// The ceiling a payload of `kind` is bounded by; an operation takes its mergeable-document's.
pub(crate) fn max_payload(kind: PodRecordKind) -> u64 {
    match kind {
        PodRecordKind::ImmutableDocument => MAX_DOCUMENT_PAYLOAD,
        PodRecordKind::Claim | PodRecordKind::MergeableDocument => MAX_ENTRY_PAYLOAD,
    }
}

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

/// What became of a code this node minted: `by` is the joiner's identity
/// for a pod invite and the newcomer's node id for a linking payload.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum InviteState {
    Pending,
    /// Presented and burned; its dialogue in flight or cut short.
    Burned,
    Used {
        by: String,
    },
    /// Never presented, its lifetime over.
    Expired,
    /// Not minted here for what the call names, or minted before the node last came up.
    Unknown,
}

impl<N: Display> From<pdn_node::InviteState<N>> for InviteState {
    fn from(state: pdn_node::InviteState<N>) -> Self {
        match state {
            pdn_node::InviteState::Pending => Self::Pending,
            pdn_node::InviteState::Burned => Self::Burned,
            pdn_node::InviteState::Used(by) => Self::Used { by: by.to_string() },
            pdn_node::InviteState::Expired => Self::Expired,
            pdn_node::InviteState::Unknown => Self::Unknown,
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

/// A payload or operation of a record of `kind` on its way in: bytes, unchanged, bounded.
pub(crate) fn entry_payload(kind: PodRecordKind, payload: &[u8]) -> Result<(), PdnError> {
    if payload.is_empty() {
        return Err(empty_payload());
    }
    bounded(kind, u64::try_from(payload.len()).unwrap_or(u64::MAX))
}

pub(crate) fn empty_payload() -> PdnError {
    PdnError::malformed("an empty entry payload, which the engine keeps no entry for")
}

pub(crate) fn bounded(kind: PodRecordKind, len: u64) -> Result<(), PdnError> {
    let ceiling = max_payload(kind);
    if len > ceiling {
        return Err(PdnError::malformed(format!(
            "an entry payload of {len} bytes, above the ceiling of {ceiling} for {kind:?}"
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

    /// Each kind is bounded by its own ceiling: a claim and an operation a byte past 1 MiB are
    /// refused where an immutable-document of that size is taken, and an immutable-document a
    /// byte past 100 MiB is refused too; an empty payload is refused, one byte taken. The lengths
    /// are checked without allocating them.
    #[test]
    fn each_kind_is_bounded_by_its_own_ceiling() {
        assert!(entry_payload(PodRecordKind::Claim, b"").is_err());
        assert!(entry_payload(PodRecordKind::Claim, b"a").is_ok());
        assert!(bounded(PodRecordKind::Claim, MAX_ENTRY_PAYLOAD).is_ok());
        assert!(bounded(PodRecordKind::Claim, MAX_ENTRY_PAYLOAD + 1).is_err());
        assert!(bounded(PodRecordKind::MergeableDocument, MAX_ENTRY_PAYLOAD + 1).is_err());
        assert!(bounded(PodRecordKind::ImmutableDocument, MAX_ENTRY_PAYLOAD + 1).is_ok());
        assert!(bounded(PodRecordKind::ImmutableDocument, MAX_DOCUMENT_PAYLOAD).is_ok());
        assert!(bounded(PodRecordKind::ImmutableDocument, MAX_DOCUMENT_PAYLOAD + 1).is_err());
    }
}
