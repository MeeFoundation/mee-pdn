//! The key layout of a pod's two stores, by the pod stores spec. Every
//! segment is text; a key that parses to none of these types is outside the
//! layout, kept and read by nothing.

use std::fmt;

use pdn_store::AuthorId;
use pdn_types::{PdnId, RecordId, RecordKind, RecordRef};
use serde::{Deserialize, Serialize};

const MEMBER: &str = "member";
const DEVICES: &str = "devices";
const BY: &str = "by";

/// A position in one member's chain of membership events. Ordered as a
/// number; its text form in a key is decimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// A chain's first event, the founding event or a newcomer's joined event.
    pub const FIRST: Self = Self(1);
    /// The actor sequence the founding event names: the creator holds no
    /// point of its chain before it.
    pub const BEFORE_FOUNDING: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A membership event's kind, its key's `<kind>` segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    Founded,
    Joined,
    Left,
    Kicked,
    Promoted,
    Demoted,
}

impl EventKind {
    const ALL: [Self; 6] = [
        Self::Founded,
        Self::Joined,
        Self::Left,
        Self::Kicked,
        Self::Promoted,
        Self::Demoted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Founded => "founded",
            Self::Joined => "joined",
            Self::Left => "left",
            Self::Kicked => "kicked",
            Self::Promoted => "promoted",
            Self::Demoted => "demoted",
        }
    }

    fn parse(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == segment)
    }
}

/// A key of the membership store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MembershipKey {
    /// `member/<subject>/<seq>/<kind>/<actor>/<actor_seq>`: the entry's
    /// author counts only among `actor`'s devices.
    Event {
        subject: PdnId,
        seq: Seq,
        kind: EventKind,
        actor: PdnId,
        actor_seq: Seq,
    },
    /// `member/<member>/devices/<version>`, one key per version.
    Devices { member: PdnId, version: u64 },
}

impl MembershipKey {
    /// The founding event's key: the creator's first sequence, naming the
    /// creator as its actor at [`Seq::BEFORE_FOUNDING`].
    pub const fn founded(creator: PdnId) -> Self {
        Self::Event {
            subject: creator,
            seq: Seq::FIRST,
            kind: EventKind::Founded,
            actor: creator,
            actor_seq: Seq::BEFORE_FOUNDING,
        }
    }

    pub fn parse(key: &[u8]) -> Option<Self> {
        let segments: Vec<&str> = std::str::from_utf8(key).ok()?.split('/').collect();
        match segments.as_slice() {
            [MEMBER, member, DEVICES, version] => Some(Self::Devices {
                member: pdn_id(member)?,
                version: number(version)?,
            }),
            [MEMBER, subject, seq, kind, actor, actor_seq] => Some(Self::Event {
                subject: pdn_id(subject)?,
                seq: Seq::new(number(seq)?),
                kind: EventKind::parse(kind)?,
                actor: pdn_id(actor)?,
                actor_seq: Seq::new(number(actor_seq)?),
            }),
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

impl fmt::Display for MembershipKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event {
                subject,
                seq,
                kind,
                actor,
                actor_seq,
            } => write!(
                f,
                "{MEMBER}/{subject}/{seq}/{}/{actor}/{actor_seq}",
                kind.as_str()
            ),
            Self::Devices { member, version } => write!(f, "{MEMBER}/{member}/{DEVICES}/{version}"),
        }
    }
}

/// An operation's `<op>` segment, `<writer>.<author>.<mseq>.<op_seq>`: the
/// writer the operation reads as, the author that signs it, the writer's
/// membership sequence and the author's own count of its operations on the
/// record, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId {
    pub writer: PdnId,
    pub author: AuthorId,
    pub mseq: Seq,
    pub op_seq: u64,
}

impl OpId {
    fn parse(segment: &str) -> Option<Self> {
        let parts: Vec<&str> = segment.split('.').collect();
        let &[writer, author, mseq, op_seq] = parts.as_slice() else {
            return None;
        };
        Some(Self {
            writer: pdn_id(writer)?,
            author: AuthorId::from(&lower_hex::<32>(author)?),
            mseq: Seq::new(number(mseq)?),
            op_seq: number(op_seq)?,
        })
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            writer,
            author,
            mseq,
            op_seq,
        } = self;
        write!(f, "{writer}.{author}.{mseq}.{op_seq}")
    }
}

/// A key of the record store: `by/<member>/<kind>/<id>/` and one last
/// segment, the writer's membership sequence or, for a mergeable-document,
/// the operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecordKey {
    Claim {
        member: PdnId,
        id: RecordId,
        mseq: Seq,
    },
    ImmutableDocument {
        member: PdnId,
        id: RecordId,
        mseq: Seq,
    },
    Operation {
        member: PdnId,
        id: RecordId,
        op: OpId,
    },
}

impl RecordKey {
    pub fn record(&self) -> RecordRef {
        let (member, kind, id) = match *self {
            Self::Claim { member, id, .. } => (member, RecordKind::Claim, id),
            Self::ImmutableDocument { member, id, .. } => {
                (member, RecordKind::ImmutableDocument, id)
            }
            Self::Operation { member, id, .. } => (member, RecordKind::MergeableDocument, id),
        };
        RecordRef { member, kind, id }
    }

    pub fn parse(key: &[u8]) -> Option<Self> {
        let segments: Vec<&str> = std::str::from_utf8(key).ok()?.split('/').collect();
        let &[BY, member, kind, id, last] = segments.as_slice() else {
            return None;
        };
        let (member, id) = (pdn_id(member)?, RecordId::from_bytes(lower_hex(id)?));
        match kind.parse().ok()? {
            RecordKind::Claim => Some(Self::Claim {
                member,
                id,
                mseq: Seq::new(number(last)?),
            }),
            RecordKind::ImmutableDocument => Some(Self::ImmutableDocument {
                member,
                id,
                mseq: Seq::new(number(last)?),
            }),
            RecordKind::MergeableDocument => Some(Self::Operation {
                member,
                id,
                op: OpId::parse(last)?,
            }),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = RecordPrefix(self.record());
        match self {
            Self::Claim { mseq, .. } | Self::ImmutableDocument { mseq, .. } => {
                write!(f, "{prefix}{mseq}")
            }
            Self::Operation { op, .. } => write!(f, "{prefix}{op}"),
        }
    }
}

/// The prefix every entry of `record` sits under.
pub fn record_prefix(record: &RecordRef) -> Vec<u8> {
    RecordPrefix(*record).to_string().into_bytes()
}

struct RecordPrefix(RecordRef);

impl fmt::Display for RecordPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let RecordRef { member, kind, id } = self.0;
        write!(f, "{BY}/{member}/{kind}/{id}/")
    }
}

fn pdn_id(segment: &str) -> Option<PdnId> {
    lower_hex(segment).map(PdnId::from_bytes)
}

/// Lowercase only: an uppercase spelling would be a second key naming the
/// same member.
fn lower_hex<const N: usize>(segment: &str) -> Option<[u8; N]> {
    if !segment
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    pdn_types::parse_hex(segment).ok()
}

/// Decimal with no leading zeros, for the reason `lower_hex` refuses
/// uppercase. Parsed rather than compared as text: the store orders `10`
/// before `9`.
fn number(segment: &str) -> Option<u64> {
    let canonical = segment == "0"
        || (!segment.starts_with('0')
            && !segment.is_empty()
            && segment.bytes().all(|b| b.is_ascii_digit()));
    if !canonical {
        return None;
    }
    segment.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "65bcff20d2b149925daa94e3750937044e8ef27385d24cb6cb7bf4182b408ba5";
    const BOB: &str = "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0";
    const CAROL: &str = "ca401ca401ca401ca401ca401ca401ca401ca401ca401ca401ca401ca401ca40";
    const B2_AUTHOR: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
    const ID: &str = "1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d";

    fn id(hex: &str) -> PdnId {
        hex.parse().unwrap()
    }

    /// Every layout the pod stores spec prints parses to what its segments
    /// name and prints back byte for byte.
    #[test]
    fn every_layout_round_trips() {
        let alice_founded = format!("member/{ALICE}/1/founded/{ALICE}/0");
        assert_eq!(
            MembershipKey::parse(alice_founded.as_bytes()),
            Some(MembershipKey::founded(id(ALICE)))
        );
        let carol_joined = format!("member/{CAROL}/1/joined/{BOB}/2");
        assert_eq!(
            MembershipKey::parse(carol_joined.as_bytes()),
            Some(MembershipKey::Event {
                subject: id(CAROL),
                seq: Seq::new(1),
                kind: EventKind::Joined,
                actor: id(BOB),
                actor_seq: Seq::new(2),
            })
        );
        let carol_devices = format!("member/{CAROL}/devices/1");
        assert_eq!(
            MembershipKey::parse(carol_devices.as_bytes()),
            Some(MembershipKey::Devices {
                member: id(CAROL),
                version: 1,
            })
        );
        let record_id: RecordId = ID.parse().unwrap();
        let claim = format!("by/{CAROL}/claim/{ID}/1");
        assert_eq!(
            RecordKey::parse(claim.as_bytes()),
            Some(RecordKey::Claim {
                member: id(CAROL),
                id: record_id,
                mseq: Seq::new(1),
            })
        );
        let scan = format!("by/{CAROL}/immutable-document/{ID}/1");
        assert_eq!(
            RecordKey::parse(scan.as_bytes()),
            Some(RecordKey::ImmutableDocument {
                member: id(CAROL),
                id: record_id,
                mseq: Seq::new(1),
            })
        );
        let op = format!("by/{CAROL}/mergeable-document/{ID}/{BOB}.{B2_AUTHOR}.2.7");
        assert_eq!(
            RecordKey::parse(op.as_bytes()),
            Some(RecordKey::Operation {
                member: id(CAROL),
                id: record_id,
                op: OpId {
                    writer: id(BOB),
                    author: AuthorId::from(&pdn_types::parse_hex(B2_AUTHOR).unwrap()),
                    mseq: Seq::new(2),
                    op_seq: 7,
                },
            })
        );
        for key in [&alice_founded, &carol_joined, &carol_devices] {
            let parsed = MembershipKey::parse(key.as_bytes()).unwrap();
            assert_eq!(parsed.to_bytes(), key.as_bytes());
        }
        for key in [&claim, &scan, &op] {
            let parsed = RecordKey::parse(key.as_bytes()).unwrap();
            assert_eq!(parsed.to_bytes(), key.as_bytes());
        }
    }

    /// A chain past sequence 9 parses in number order, although the store
    /// orders its keys `…/10/…` before `…/9/…`.
    #[test]
    fn numbers_order_as_numbers() {
        let nine = format!("member/{BOB}/9/promoted/{ALICE}/1");
        let ten = format!("member/{BOB}/10/demoted/{ALICE}/1");
        assert!(ten.as_bytes() < nine.as_bytes());
        let seq = |key: &str| match MembershipKey::parse(key.as_bytes()) {
            Some(MembershipKey::Event { seq, .. }) => Some(seq),
            _ => None,
        };
        assert!(seq(&ten).unwrap() > seq(&nine).unwrap());
    }

    /// A record's identity is its key without the last segment, whatever its
    /// kind: every key of one record sits under that record's prefix.
    #[test]
    fn record_identity_is_the_key_without_its_last_segment() {
        for key in [
            format!("by/{BOB}/claim/{ID}/3"),
            format!("by/{BOB}/immutable-document/{ID}/1"),
            format!("by/{BOB}/mergeable-document/{ID}/{CAROL}.{B2_AUTHOR}.1.1"),
        ] {
            let record = RecordKey::parse(key.as_bytes()).unwrap().record();
            let (without_last, _) = key.rsplit_once('/').unwrap();
            assert_eq!(
                record_prefix(&record),
                format!("{without_last}/").as_bytes()
            );
        }
    }

    /// Keys that fit neither layout, or one only in part or in a
    /// non-canonical spelling, parse to nothing in either store; a record
    /// key parses to nothing as a membership key and the other way round.
    #[test]
    fn keys_outside_the_layout_parse_to_nothing() {
        let upper = ALICE.to_uppercase();
        let membership_misses = [
            "ext/anything".to_owned(),
            format!("member/{ALICE}/01/founded/{ALICE}/0"),
            format!("member/{ALICE}/+1/founded/{ALICE}/0"),
            format!("member/{ALICE}/1/founded/{ALICE}/"),
            format!("member/{upper}/1/founded/{upper}/0"),
            format!("member/{ALICE}/1/suspended/{ALICE}/0"),
            format!("member/{ALICE}/1/founded/{ALICE}/0/extra"),
            format!("member/{ALICE}/devices/"),
            format!("member/{ALICE}/devices/1/2"),
            format!("member/{ALICE}/18446744073709551616/left/{ALICE}/1"),
            format!("by/{ALICE}/claim/{ID}/1"),
        ];
        for key in &membership_misses {
            assert_eq!(MembershipKey::parse(key.as_bytes()), None, "{key}");
        }
        let record_misses = [
            "ext/anything".to_owned(),
            format!("by/{BOB}/"),
            format!("by/{BOB}/claim/{ID}"),
            format!("by/{BOB}/claim/{ID}/{CAROL}.{B2_AUTHOR}.1.1"),
            format!("by/{BOB}/mergeable-document/{ID}/1"),
            format!("by/{BOB}/mergeable-document/{ID}/{CAROL}.{B2_AUTHOR}.1"),
            format!("by/{BOB}/mergeable-document/{ID}/{CAROL}.{B2_AUTHOR}.1.1.1"),
            format!("by/{BOB}/mergeable-document/{ID}/{CAROL}.{B2_AUTHOR}.1.01"),
            format!("by/{BOB}/Claim/{ID}/1"),
            format!("by/{BOB}/claim/{ALICE}/1"),
            format!("member/{ALICE}/1/founded/{ALICE}/0"),
        ];
        for key in &record_misses {
            assert_eq!(RecordKey::parse(key.as_bytes()), None, "{key}");
        }
        assert_eq!(MembershipKey::parse(&[0xff, b'/']), None);
    }
}
