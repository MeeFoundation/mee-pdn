//! The membership view: what the entries a device holds in a pod's
//! membership store make of its members, and what each entry counts for,
//! whatever order they arrived in — by the pod stores spec's requirements
//! on the membership store and on device statements.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use pdn_store::AuthorId;
use pdn_types::{PdnId, PodId};

use super::{
    keys::{EventKind, MembershipKey, Seq},
    payloads::{CreatedPayload, DevicesPayload, JoinedPayload, MemberDevice},
};
use crate::announcement::{creation_verifies, devices_verify, join_verifies, pdn_id_of, pod_id_of};

/// One entry of a membership store as a device holds it; `payload` is
/// `None` until its bytes have arrived.
#[derive(Clone, Debug)]
pub struct HeldEntry {
    pub key: Vec<u8>,
    pub author: AuthorId,
    pub payload: Option<Vec<u8>>,
}

/// What one held entry counts for, over everything the device holds: in the
/// membership store by the membership view, in the record store by the
/// record view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Counted,
    CountedForNothing(ForNothing),
    NotYet(Awaiting),
    /// The key fits no layout of its store: kept, and read by nothing.
    OutsideLayout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForNothing {
    /// The payload does not decode.
    Malformed,
    /// A created event anywhere but the creator's sequence 1 naming its
    /// creator at 0, or an event at sequence 0.
    Misplaced,
    /// The created event's fields derive another pod id.
    OtherPod,
    /// The announcement key it carries derives another `PdnId` than its
    /// subject's.
    KeyOfAnother,
    BadSignature,
    /// A join, removal or demotion whose actor is its subject; a leave whose
    /// actor is not.
    WrongActor,
    /// Its author is no device of the actor, or the writer, its key names.
    AuthorNotActorDevice,
    /// An operation whose author is not the one its key names.
    AuthorNotNamed,
    /// The actor, or the writer, lacks at the point its key names the state
    /// the entry needs.
    ActorLacksState,
    /// The subject's state before its sequence allows no such transition.
    TransitionNotAllowed,
    /// What it rests on leads back to itself.
    Cyclic,
    /// A demotion set aside so the pod keeps an owner.
    LastOwnerKept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awaiting {
    Payload,
    /// The actor's or the writer's chain is not held up to the point its
    /// key names.
    ActorChain,
    /// The subject's chain is not held up to the sequence before the
    /// event's.
    SubjectChain,
    /// No held event carries the member's announcement key.
    AnnouncementKey,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemberState {
    pub member: bool,
    pub owner: bool,
}

/// An identity the membership store names, as the membership view shows it.
#[derive(Clone, Debug, Default)]
pub struct Member {
    /// After its chain as far as the first sequence holding no entry.
    pub state: MemberState,
    pub announcement_key: Option<[u8; 32]>,
    /// The union of its counted device statements.
    pub devices: BTreeSet<MemberDevice>,
    /// The highest version among its counted device statements; `0` for
    /// none. The next statement takes the version after it.
    pub statement_version: u64,
    /// The state after each sequence of that run, from 1.
    chain: BTreeMap<u64, MemberState>,
}

impl Member {
    /// The last sequence of the member's chain before the first one the
    /// device holds no entry at; `0` for no chain. The next event of the
    /// chain goes at the sequence after it.
    pub fn run(&self) -> u64 {
        self.chain.keys().next_back().copied().unwrap_or(0)
    }

    /// The sequence at which the member last became one — its created or
    /// its latest joined event — while it is a member.
    pub fn joined_at(&self) -> Option<u64> {
        if !self.state.member {
            return None;
        }
        (1..=self.run())
            .rev()
            .find(|seq| !self.state_at(seq - 1).is_some_and(|before| before.member))
    }

    /// `None` past the run of sequences the device holds.
    pub fn state_at(&self, seq: u64) -> Option<MemberState> {
        if seq == 0 {
            return Some(MemberState::default());
        }
        self.chain.get(&seq).copied()
    }
}

/// What a device's entries make of a pod's members, and each entry's
/// verdict in the order the entries were given.
#[derive(Clone, Debug)]
pub struct MembershipView {
    members: BTreeMap<PdnId, Member>,
    verdicts: Vec<Verdict>,
}

impl MembershipView {
    pub fn new(pod: &PodId, entries: &[HeldEntry]) -> Self {
        let parsed = Parsed::of(entries);
        let mut set_aside = BTreeSet::new();
        loop {
            let pass = Pass::run(pod, entries, &parsed, &set_aside);
            let more = pass.demotions_to_set_aside();
            if more.is_subset(&set_aside) {
                return pass.into_view();
            }
            set_aside.extend(more);
        }
    }

    pub fn member(&self, id: &PdnId) -> Option<&Member> {
        self.members.get(id)
    }

    /// Every identity the store names, member or not.
    pub fn identities(&self) -> impl Iterator<Item = (&PdnId, &Member)> {
        self.members.iter()
    }

    pub fn verdicts(&self) -> &[Verdict] {
        &self.verdicts
    }
}

#[derive(Clone, Copy, Debug)]
struct Event {
    entry: usize,
    subject: PdnId,
    seq: u64,
    kind: EventKind,
    actor: PdnId,
    actor_seq: u64,
}

#[derive(Clone, Copy, Debug)]
struct Statement {
    entry: usize,
    member: PdnId,
    version: u64,
}

/// The entries by their keys, and which sequences of each chain hold one,
/// whatever it counts for.
struct Parsed {
    events: Vec<Event>,
    statements: Vec<Statement>,
    present: HashMap<PdnId, BTreeSet<u64>>,
    /// Per chain, the last sequence before the first one holding no entry.
    runs: HashMap<PdnId, u64>,
}

impl Parsed {
    fn of(entries: &[HeldEntry]) -> Self {
        let mut parsed = Self {
            events: Vec::new(),
            statements: Vec::new(),
            present: HashMap::new(),
            runs: HashMap::new(),
        };
        for (entry, held) in entries.iter().enumerate() {
            match MembershipKey::parse(&held.key) {
                Some(MembershipKey::Event {
                    subject,
                    seq,
                    kind,
                    actor,
                    actor_seq,
                }) => {
                    parsed.present.entry(subject).or_default().insert(seq.get());
                    parsed.events.push(Event {
                        entry,
                        subject,
                        seq: seq.get(),
                        kind,
                        actor,
                        actor_seq: actor_seq.get(),
                    });
                }
                Some(MembershipKey::Devices { member, version }) => {
                    parsed.statements.push(Statement {
                        entry,
                        member,
                        version,
                    });
                }
                None => {}
            }
        }
        for (member, held) in &parsed.present {
            let mut run = 0;
            while held.contains(&(run + 1)) {
                run += 1;
            }
            parsed.runs.insert(*member, run);
        }
        parsed
    }

    /// Whether every sequence from 1 to `upto` of `member`'s chain holds an
    /// entry.
    fn holds(&self, member: &PdnId, upto: u64) -> bool {
        upto <= self.run(member)
    }

    fn run(&self, member: &PdnId) -> u64 {
        self.runs.get(member).copied().unwrap_or(0)
    }
}

/// The counted events of the highest rank at one sequence of one chain.
#[derive(Clone, Debug)]
struct Winner {
    kind: EventKind,
    events: Vec<(usize, PdnId)>,
}

/// One evaluation of every entry, with `set_aside` the demotions the guard
/// over the last owner ignores.
struct Pass<'a> {
    pod: &'a PodId,
    entries: &'a [HeldEntry],
    parsed: &'a Parsed,
    set_aside: &'a BTreeSet<usize>,
    keys: HashMap<PdnId, [u8; 32]>,
    devices: HashMap<PdnId, BTreeSet<MemberDevice>>,
    statement_versions: HashMap<PdnId, u64>,
    authors: HashMap<PdnId, HashSet<AuthorId>>,
    verdicts: Vec<Verdict>,
    winners: HashMap<PdnId, BTreeMap<u64, Winner>>,
    /// The state after each sequence of each chain's run, once judged.
    states: HashMap<(PdnId, u64), MemberState>,
}

impl<'a> Pass<'a> {
    fn run(
        pod: &'a PodId,
        entries: &'a [HeldEntry],
        parsed: &'a Parsed,
        set_aside: &'a BTreeSet<usize>,
    ) -> Self {
        let mut pass = Self {
            pod,
            entries,
            parsed,
            set_aside,
            keys: HashMap::new(),
            devices: HashMap::new(),
            statement_versions: HashMap::new(),
            authors: HashMap::new(),
            verdicts: vec![Verdict::OutsideLayout; entries.len()],
            winners: HashMap::new(),
            states: HashMap::new(),
        };
        pass.find_keys();
        pass.count_statements();
        let candidates = pass.check_alone();
        pass.evaluate(&candidates);
        pass
    }

    fn payload(&self, entry: usize) -> Option<&[u8]> {
        self.entries.get(entry)?.payload.as_deref()
    }

    fn author(&self, entry: usize) -> Option<AuthorId> {
        self.entries.get(entry).map(|held| held.author)
    }

    fn set(&mut self, entry: usize, verdict: Verdict) {
        if let Some(slot) = self.verdicts.get_mut(entry) {
            *slot = verdict;
        }
    }

    /// A member's announcement key is the one any held created or joined
    /// event in its chain carries that derives the member's `PdnId`.
    fn find_keys(&mut self) {
        for event in &self.parsed.events {
            let Some(payload) = self.payload(event.entry) else {
                continue;
            };
            let key = match event.kind {
                EventKind::Created => CreatedPayload::decode(payload).map(|p| p.announcement_key),
                EventKind::Joined => JoinedPayload::decode(payload).map(|p| p.announcement_key),
                _ => None,
            };
            if let Some(key) = key.filter(|key| pdn_id_of(key) == event.subject) {
                self.keys.insert(event.subject, key);
            }
        }
    }

    fn count_statements(&mut self) {
        for statement in &self.parsed.statements {
            let verdict = self.statement_verdict(statement);
            if verdict == Verdict::Counted {
                if let Some(payload) = self
                    .payload(statement.entry)
                    .and_then(DevicesPayload::decode)
                {
                    self.authors
                        .entry(statement.member)
                        .or_default()
                        .extend(payload.devices.iter().map(|device| device.author));
                    self.devices
                        .entry(statement.member)
                        .or_default()
                        .extend(payload.devices);
                    let version = self.statement_versions.entry(statement.member).or_default();
                    *version = (*version).max(statement.version);
                }
            }
            self.set(statement.entry, verdict);
        }
    }

    fn statement_verdict(&self, statement: &Statement) -> Verdict {
        let Some(payload) = self.payload(statement.entry) else {
            return Verdict::NotYet(Awaiting::Payload);
        };
        let Some(payload) = DevicesPayload::decode(payload) else {
            return Verdict::CountedForNothing(ForNothing::Malformed);
        };
        let Some(key) = self.keys.get(&statement.member) else {
            return Verdict::NotYet(Awaiting::AnnouncementKey);
        };
        if devices_verify(key, statement.version, &payload) {
            Verdict::Counted
        } else {
            Verdict::CountedForNothing(ForNothing::BadSignature)
        }
    }

    /// Every check that reads no member's state; the events that pass them
    /// all are what the state-reading checks see.
    fn check_alone(&mut self) -> Vec<Event> {
        let mut candidates = Vec::new();
        for event in self.parsed.events.clone() {
            match self.alone(&event) {
                Ok(()) => candidates.push(event),
                Err(verdict) => self.set(event.entry, verdict),
            }
        }
        candidates
    }

    fn alone(&self, event: &Event) -> Result<(), Verdict> {
        let nothing = |why| Err(Verdict::CountedForNothing(why));
        if event.seq == 0 {
            return nothing(ForNothing::Misplaced);
        }
        match event.kind {
            EventKind::Created => self.creation_alone(event)?,
            EventKind::Joined => self.joining_alone(event)?,
            EventKind::Left if event.actor != event.subject => {
                return nothing(ForNothing::WrongActor)
            }
            EventKind::Removed | EventKind::Demoted if event.actor == event.subject => {
                return nothing(ForNothing::WrongActor);
            }
            _ => {}
        }
        let authored = self.author(event.entry).is_some_and(|author| {
            self.authors
                .get(&event.actor)
                .is_some_and(|authors| authors.contains(&author))
        });
        if !authored {
            return nothing(ForNothing::AuthorNotActorDevice);
        }
        if !self.parsed.holds(&event.actor, event.actor_seq) {
            return Err(Verdict::NotYet(Awaiting::ActorChain));
        }
        if !self.parsed.holds(&event.subject, event.seq - 1) {
            return Err(Verdict::NotYet(Awaiting::SubjectChain));
        }
        Ok(())
    }

    fn creation_alone(&self, event: &Event) -> Result<(), Verdict> {
        let nothing = |why| Err(Verdict::CountedForNothing(why));
        if event.seq != 1 || event.actor != event.subject || event.actor_seq != 0 {
            return nothing(ForNothing::Misplaced);
        }
        let Some(payload) = self.payload(event.entry) else {
            return Err(Verdict::NotYet(Awaiting::Payload));
        };
        let Some(creation) = CreatedPayload::decode(payload) else {
            return nothing(ForNothing::Malformed);
        };
        if pod_id_of(&event.subject, &creation.announcement_key, &creation.nonce) != *self.pod {
            return nothing(ForNothing::OtherPod);
        }
        if pdn_id_of(&creation.announcement_key) != event.subject {
            return nothing(ForNothing::KeyOfAnother);
        }
        if !creation_verifies(&event.subject, &creation) {
            return nothing(ForNothing::BadSignature);
        }
        Ok(())
    }

    fn joining_alone(&self, event: &Event) -> Result<(), Verdict> {
        let nothing = |why| Err(Verdict::CountedForNothing(why));
        if event.actor == event.subject {
            return nothing(ForNothing::WrongActor);
        }
        let Some(payload) = self.payload(event.entry) else {
            return Err(Verdict::NotYet(Awaiting::Payload));
        };
        let Some(statement) = JoinedPayload::decode(payload) else {
            return nothing(ForNothing::Malformed);
        };
        if pdn_id_of(&statement.announcement_key) != event.subject {
            return nothing(ForNothing::KeyOfAnother);
        }
        if !join_verifies(&event.subject, self.pod, Seq::new(event.seq), &statement) {
            return nothing(ForNothing::BadSignature);
        }
        Ok(())
    }

    /// The state-reading checks, each event once the states it reads are
    /// known. A chain's sequences settle in order; at each, an event whose
    /// transition the state before it does not allow counts for nothing at
    /// once, and the others wait for their actor's point. Events left
    /// waiting on each other in a loop, which only a modified device
    /// writes, count for nothing, and what waited on them settles after.
    fn evaluate(&mut self, candidates: &[Event]) {
        let mut settling = Settling::default();
        for (index, event) in candidates.iter().enumerate() {
            settling
                .at
                .entry((event.subject, event.seq))
                .or_default()
                .push(index);
        }
        for (member, run) in &self.parsed.runs {
            if *run >= 1 {
                settling.open.push((*member, 1));
            }
        }
        loop {
            if let Some((member, seq)) = settling.close.pop() {
                self.close_sequence(candidates, &mut settling, member, seq);
            } else if let Some((member, seq)) = settling.open.pop() {
                self.open_sequence(candidates, &mut settling, member, seq);
            } else if settling.awaited.is_empty() {
                return;
            } else {
                self.break_loops(candidates, &mut settling);
            }
        }
    }

    fn open_sequence(
        &mut self,
        candidates: &[Event],
        settling: &mut Settling,
        member: PdnId,
        seq: u64,
    ) {
        let before = self.state(&member, seq - 1);
        let mut pending = 0;
        let here = settling.at.get(&(member, seq)).cloned().unwrap_or_default();
        for index in here {
            let Some(event) = candidates.get(index).copied() else {
                continue;
            };
            if !allows(before, event.kind) {
                self.decide(
                    &event,
                    Verdict::CountedForNothing(ForNothing::TransitionNotAllowed),
                );
            } else if let Some(actor) = self.actor_state(&event) {
                let verdict = self.judge(&event, actor);
                self.decide(&event, verdict);
            } else {
                let point = (event.actor, event.actor_seq);
                settling.waiting.entry(point).or_default().push(index);
                settling.awaited.insert(index, point);
                pending += 1;
            }
        }
        if pending == 0 {
            settling.close.push((member, seq));
        } else {
            settling.pending.insert((member, seq), pending);
        }
    }

    fn close_sequence(
        &mut self,
        candidates: &[Event],
        settling: &mut Settling,
        member: PdnId,
        seq: u64,
    ) {
        let before = self.state(&member, seq - 1);
        let after = self
            .winners
            .get(&member)
            .and_then(|chain| chain.get(&seq))
            .map_or(before, |winner| apply(winner.kind));
        self.states.insert((member, seq), after);
        settling.settled.insert(member, seq);
        for index in settling.waiting.remove(&(member, seq)).unwrap_or_default() {
            settling.awaited.remove(&index);
            let Some(event) = candidates.get(index).copied() else {
                continue;
            };
            let verdict = self.judge(&event, after);
            self.decide(&event, verdict);
            settling.one_less_pending(event.subject, event.seq);
        }
        if seq < self.parsed.run(&member) {
            settling.open.push((member, seq + 1));
        }
    }

    /// Every waiting event waits on the first unsettled sequence of its
    /// actor's chain, and so on the events still waiting there; those on a
    /// loop of such waits count for nothing.
    fn break_loops(&mut self, candidates: &[Event], settling: &mut Settling) {
        let waiting: Vec<usize> = settling.awaited.keys().copied().collect();
        let node_of: HashMap<usize, usize> = waiting
            .iter()
            .enumerate()
            .map(|(node, index)| (*index, node))
            .collect();
        let mut waits_on: HashMap<(PdnId, u64), Vec<usize>> = HashMap::new();
        for index in &waiting {
            if let Some(event) = candidates.get(*index) {
                waits_on
                    .entry((event.subject, event.seq))
                    .or_default()
                    .push(node_of.get(index).copied().unwrap_or_default());
            }
        }
        let adjacency: Vec<Vec<usize>> = waiting
            .iter()
            .map(|index| {
                let Some((actor, _point)) = settling.awaited.get(index) else {
                    return Vec::new();
                };
                let unsettled = settling.settled.get(actor).copied().unwrap_or(0) + 1;
                waits_on
                    .get(&(*actor, unsettled))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        for component in components(&adjacency) {
            let looped = match component.as_slice() {
                [single] => adjacency
                    .get(*single)
                    .is_some_and(|next| next.contains(single)),
                _ => true,
            };
            if !looped {
                continue;
            }
            for node in component {
                let Some(&index) = waiting.get(node) else {
                    continue;
                };
                let Some(event) = candidates.get(index).copied() else {
                    continue;
                };
                if let Some(point) = settling.awaited.remove(&index) {
                    if let Some(queue) = settling.waiting.get_mut(&point) {
                        queue.retain(|other| *other != index);
                    }
                }
                self.decide(&event, Verdict::CountedForNothing(ForNothing::Cyclic));
                settling.one_less_pending(event.subject, event.seq);
            }
        }
    }

    /// `None` while the point the event names is unsettled.
    fn actor_state(&self, event: &Event) -> Option<MemberState> {
        match event.kind {
            EventKind::Created | EventKind::Left => Some(MemberState::default()),
            _ if event.actor_seq == 0 => Some(MemberState::default()),
            _ => self.states.get(&(event.actor, event.actor_seq)).copied(),
        }
    }

    fn decide(&mut self, event: &Event, verdict: Verdict) {
        if verdict == Verdict::Counted {
            self.record_winner(event);
        }
        self.set(event.entry, verdict);
    }

    fn state(&self, member: &PdnId, seq: u64) -> MemberState {
        if seq == 0 {
            return MemberState::default();
        }
        self.states
            .get(&(*member, seq))
            .copied()
            .unwrap_or_default()
    }

    /// The checks on the actor's state at its named point; the transition
    /// is checked where the event's sequence opens.
    fn judge(&self, event: &Event, actor: MemberState) -> Verdict {
        let needed = match event.kind {
            EventKind::Joined => actor.member,
            EventKind::Promoted | EventKind::Removed | EventKind::Demoted => actor.owner,
            EventKind::Created | EventKind::Left => true,
        };
        if !needed {
            return Verdict::CountedForNothing(ForNothing::ActorLacksState);
        }
        if self.set_aside.contains(&event.entry) {
            return Verdict::CountedForNothing(ForNothing::LastOwnerKept);
        }
        Verdict::Counted
    }

    fn record_winner(&mut self, event: &Event) {
        let at_seq = self.winners.entry(event.subject).or_default();
        match at_seq.get_mut(&event.seq) {
            Some(winner) if rank(winner.kind) > rank(event.kind) => {}
            Some(winner) if winner.kind == event.kind => {
                winner.events.push((event.entry, event.actor));
            }
            _ => {
                at_seq.insert(
                    event.seq,
                    Winner {
                        kind: event.kind,
                        events: vec![(event.entry, event.actor)],
                    },
                );
            }
        }
    }

    /// The guard over the last owner: with no owner left, the demotions
    /// that ended the role of a former owner still a member, taken in the
    /// order of their actors' `PdnId`s, and those that would demote the last
    /// of them.
    fn demotions_to_set_aside(&self) -> BTreeSet<usize> {
        let finals: Vec<(PdnId, MemberState)> = self
            .identities()
            .map(|id| (id, self.state(&id, self.parsed.run(&id))))
            .collect();
        if finals.iter().any(|(_id, state)| state.owner) {
            return BTreeSet::new();
        }
        let mut ended: Vec<(PdnId, PdnId, Vec<usize>)> = finals
            .iter()
            .filter(|(_id, state)| state.member)
            .filter_map(|(id, _state)| {
                self.ending_demotion(id)
                    .map(|(actor, entries)| (actor, *id, entries))
            })
            .collect();
        ended.sort_by_key(|(actor, subject, _entries)| (*actor, *subject));
        let mut remaining: BTreeSet<PdnId> =
            ended.iter().map(|(_a, subject, _e)| *subject).collect();
        let mut set_aside = BTreeSet::new();
        for (_actor, subject, entries) in ended {
            if remaining.len() == 1 && remaining.contains(&subject) {
                set_aside.extend(entries);
            } else {
                remaining.remove(&subject);
            }
        }
        set_aside
    }

    /// The demotion that last took `member`'s ownership, if nothing since
    /// gave it back or took the member out: its lowest actor and its
    /// entries.
    fn ending_demotion(&self, member: &PdnId) -> Option<(PdnId, Vec<usize>)> {
        let chain = self.winners.get(member)?;
        let mut ending = None;
        for winner in chain.range(1..=self.parsed.run(member)).map(|(_seq, w)| w) {
            ending = match winner.kind {
                EventKind::Demoted => {
                    let actor = winner.events.iter().map(|(_entry, actor)| *actor).min()?;
                    Some((
                        actor,
                        winner.events.iter().map(|(entry, _a)| *entry).collect(),
                    ))
                }
                _ => None,
            };
        }
        ending
    }

    fn identities(&self) -> impl Iterator<Item = PdnId> + '_ {
        let mut ids: BTreeSet<PdnId> = self.parsed.present.keys().copied().collect();
        ids.extend(
            self.parsed
                .statements
                .iter()
                .map(|statement| statement.member),
        );
        ids.into_iter()
    }

    fn into_view(self) -> MembershipView {
        let members = self
            .identities()
            .map(|id| {
                let run = self.parsed.run(&id);
                let chain = (1..=run).map(|seq| (seq, self.state(&id, seq))).collect();
                let member = Member {
                    state: self.state(&id, run),
                    announcement_key: self.keys.get(&id).copied(),
                    devices: self.devices.get(&id).cloned().unwrap_or_default(),
                    statement_version: self.statement_versions.get(&id).copied().unwrap_or(0),
                    chain,
                };
                (id, member)
            })
            .collect();
        MembershipView {
            members,
            verdicts: self.verdicts,
        }
    }
}

/// What `Pass::evaluate` tracks while sequences settle.
#[derive(Default)]
struct Settling {
    /// The candidates at each sequence of each chain.
    at: HashMap<(PdnId, u64), Vec<usize>>,
    /// Per open sequence, its candidates still waiting on their actor.
    pending: HashMap<(PdnId, u64), usize>,
    /// Per point, the candidates waiting for its state.
    waiting: HashMap<(PdnId, u64), Vec<usize>>,
    /// Per waiting candidate, the point it waits for.
    awaited: HashMap<usize, (PdnId, u64)>,
    /// Per chain, the last sequence whose state is known; they settle in
    /// order.
    settled: HashMap<PdnId, u64>,
    open: Vec<(PdnId, u64)>,
    close: Vec<(PdnId, u64)>,
}

impl Settling {
    fn one_less_pending(&mut self, member: PdnId, seq: u64) {
        if let Some(pending) = self.pending.get_mut(&(member, seq)) {
            *pending = pending.saturating_sub(1);
            if *pending == 0 {
                self.pending.remove(&(member, seq));
                self.close.push((member, seq));
            }
        }
    }
}

/// Whether the state before a sequence allows the event's transition.
fn allows(before: MemberState, kind: EventKind) -> bool {
    match kind {
        EventKind::Created | EventKind::Joined => !before.member,
        EventKind::Left | EventKind::Removed | EventKind::Promoted => before.member,
        EventKind::Demoted => before.owner,
    }
}

/// Precedence at one sequence, the narrowest state first; the created
/// event and a join compete only where the subject is no member.
fn rank(kind: EventKind) -> u8 {
    match kind {
        EventKind::Removed => 6,
        EventKind::Left => 5,
        EventKind::Demoted => 4,
        EventKind::Promoted => 3,
        EventKind::Created => 2,
        EventKind::Joined => 1,
    }
}

/// The state an allowed transition leaves.
fn apply(kind: EventKind) -> MemberState {
    match kind {
        EventKind::Created | EventKind::Promoted => MemberState {
            member: true,
            owner: true,
        },
        EventKind::Joined | EventKind::Demoted => MemberState {
            member: true,
            owner: false,
        },
        EventKind::Left | EventKind::Removed => MemberState::default(),
    }
}

/// Strongly connected components, each after every component it reaches:
/// iterative, so a store of any size fits any stack.
#[allow(clippy::indexing_slicing)] // every index is a node below `adjacency.len()`
fn components(adjacency: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let nodes = adjacency.len();
    let mut index = vec![UNVISITED; nodes];
    let mut low = vec![0; nodes];
    let mut on_stack = vec![false; nodes];
    let mut stack = Vec::new();
    let mut next = 0;
    let mut out = Vec::new();
    for root in 0..nodes {
        if index[root] != UNVISITED {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        let mut work = vec![(root, 0)];
        while let Some(top) = work.last_mut() {
            let (node, edge) = *top;
            if let Some(&succ) = adjacency[node].get(edge) {
                top.1 += 1;
                if index[succ] == UNVISITED {
                    index[succ] = next;
                    low[succ] = next;
                    next += 1;
                    stack.push(succ);
                    on_stack[succ] = true;
                    work.push((succ, 0));
                } else if on_stack[succ] {
                    low[node] = low[node].min(index[succ]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _edge)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                out.push(component);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
