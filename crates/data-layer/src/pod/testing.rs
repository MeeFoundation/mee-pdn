//! The cast and the membership store the pod modules' unit tests write.

use pdn_store::AuthorId;
use pdn_types::{NodeId, PdnId, PodId};

use super::{EventKind, HeldEntry, MemberDevice, Membership, MembershipKey, Seq};
use crate::announcement::{pod_id_of, AnnouncementKeyPair};

pub(super) struct Person {
    pub(super) keys: AnnouncementKeyPair,
    pub(super) device: MemberDevice,
}

impl Person {
    pub(super) fn new(seed: u8) -> Self {
        Self {
            keys: AnnouncementKeyPair::from_secret_bytes(&[seed; 32]),
            device: device(seed),
        }
    }

    pub(super) fn id(&self) -> PdnId {
        self.keys.pdn_id()
    }

    pub(super) fn author(&self) -> AuthorId {
        self.device.author
    }
}

pub(super) fn device(seed: u8) -> MemberDevice {
    MemberDevice {
        node: NodeId::from_bytes([seed; 32]),
        author: AuthorId::from(&[seed; 32]),
    }
}

/// Alice holds the secret of the specs' examples, so her pod is "Family".
pub(super) struct Cast {
    pub(super) alice: Person,
    pub(super) bob: Person,
    pub(super) carol: Person,
    pub(super) dave: Person,
}

impl Cast {
    pub(super) fn new() -> Self {
        Self {
            alice: Person::new(0x33),
            bob: Person::new(0xb0),
            carol: Person::new(0xc0),
            dave: Person::new(0xd0),
        }
    }
}

/// A pod's membership store as one device holds it, written entry by entry.
#[derive(Clone)]
pub(super) struct Store {
    pub(super) pod: PodId,
    pub(super) entries: Vec<HeldEntry>,
}

impl Store {
    /// Created by `creator`, its first device statement beside the created
    /// event.
    pub(super) fn created_by(creator: &Person) -> Self {
        let creation = creator.keys.creation([0x5a; 16]);
        let pod = pod_id_of(&creator.id(), &creation.announcement_key, &creation.nonce);
        let mut store = Self {
            pod,
            entries: Vec::new(),
        };
        store.write(
            MembershipKey::created(creator.id()),
            creator.author(),
            creation.encode(),
        );
        store.statement(creator, 1, &[creator.device], creator.author());
        store
    }

    pub(super) fn write(
        &mut self,
        key: MembershipKey,
        author: AuthorId,
        payload: Vec<u8>,
    ) -> usize {
        self.entries.push(HeldEntry {
            key: key.to_bytes(),
            author,
            payload: Some(payload),
        });
        self.entries.len() - 1
    }

    pub(super) fn statement(
        &mut self,
        member: &Person,
        version: u64,
        devices: &[MemberDevice],
        author: AuthorId,
    ) -> usize {
        let payload = member
            .keys
            .device_statement(version, devices.to_vec())
            .encode();
        let key = MembershipKey::Devices {
            member: member.id(),
            version,
        };
        self.write(key, author, payload)
    }

    /// `actor`'s invite act at the newcomer's `seq`, naming the actor's
    /// `actor_seq`, the newcomer's join statement inside.
    pub(super) fn join(
        &mut self,
        actor: &Person,
        actor_seq: u64,
        newcomer: &Person,
        seq: u64,
    ) -> usize {
        let statement = newcomer
            .keys
            .join_statement(&self.pod, Seq::new(seq))
            .encode();
        let key = event(newcomer.id(), seq, EventKind::Joined, actor.id(), actor_seq);
        self.write(key, actor.author(), statement)
    }

    /// A first join and the newcomer's first device statement beside it, as
    /// the join dialogue writes them.
    pub(super) fn invite(&mut self, actor: &Person, actor_seq: u64, newcomer: &Person) -> usize {
        let joined = self.join(actor, actor_seq, newcomer, 1);
        self.statement(newcomer, 1, &[newcomer.device], actor.author());
        joined
    }

    pub(super) fn act(
        &mut self,
        kind: EventKind,
        subject: &Person,
        seq: u64,
        actor: &Person,
        actor_seq: u64,
    ) -> usize {
        let key = event(subject.id(), seq, kind, actor.id(), actor_seq);
        self.write(key, actor.author(), vec![0])
    }

    pub(super) fn fold(&self) -> Membership {
        Membership::fold(&self.pod, &self.entries)
    }
}

pub(super) fn event(
    subject: PdnId,
    seq: u64,
    kind: EventKind,
    actor: PdnId,
    actor_seq: u64,
) -> MembershipKey {
    MembershipKey::Event {
        subject,
        seq: Seq::new(seq),
        kind,
        actor,
        actor_seq: Seq::new(actor_seq),
    }
}
