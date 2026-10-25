//! The engine's payload fetches: a queue per provider, at most a limit of
//! fetches in flight toward each, and the pauses of a provider whose fetch
//! stalled or failed. Specified in the data layer's
//! `reconciliation-sessions` spec, "Payload downloads toward one provider
//! are bounded" and "A download that makes no progress is handed on".

use std::collections::{HashMap, HashSet, VecDeque};

use iroh::PublicKey;
use iroh_blobs::Hash;
use n0_future::time::{Duration, Instant};
use tracing::info;

use super::{UNREACHED_RETRY_FIRST, UNREACHED_RETRY_FOR, UNREACHED_RETRY_MAX};

/// How long a fetch may go without progress before it is dropped: above a
/// connect and several round trips on the slowest path, below the 15 s
/// after which the transport abandons a quiet path.
pub(super) const NO_PROGRESS: Duration = Duration::from_secs(10);

/// Gives back what a catch-up's peak left allocated: a collection under a
/// quarter full shrinks to twice what it holds.
macro_rules! shrink_if_sparse {
    ($collection:expr) => {
        if $collection.len() < $collection.capacity() / 4 {
            $collection.shrink_to($collection.len().saturating_mul(2));
        }
    };
}

/// How a fetch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Fetched,
    /// The node's blob store held it whole before the fetch asked anyone.
    Held,
    /// Nothing arrived for [`NO_PROGRESS`].
    Stalled,
    ConnectFailed,
    /// The provider answered that it lacks the payload, whole or in part.
    Lacks,
    /// The provider failed it once the connection stood.
    Failed,
    /// The node's own blob store failed it.
    LocalFailure,
}

/// What the live actor carries out after a step: the step has already
/// counted each fetch as in flight and each payload as unrouted.
#[derive(Debug, Default)]
#[must_use]
pub(super) struct Steps {
    /// Fetches to start, each toward its provider.
    pub(super) start: Vec<(PublicKey, Hash)>,
    /// After the wait, [`Fetches::resume`] with this provider and series.
    pub(super) pause: Option<(PublicKey, u64, Duration)>,
    /// Payloads that were wanted and have no provider left to fetch them.
    pub(super) unrouted: Vec<Hash>,
}

#[derive(Debug)]
struct Wanted {
    /// The providers it may be fetched from.
    eligible: HashSet<PublicKey>,
    fetching: Option<PublicKey>,
}

#[derive(Debug, Default)]
struct Provider {
    /// Payloads in turn; one no longer wanted, in flight elsewhere or no
    /// longer eligible here is dropped when its turn comes.
    queue: VecDeque<Hash>,
    /// The wanted payloads this provider is eligible for.
    holds: HashSet<Hash>,
    in_flight: usize,
    series: Option<Series>,
    /// The last fetches from it that did not land, each with what went
    /// wrong, for the report: at most [`ENDS_KEPT`].
    ends: VecDeque<String>,
}

/// How many fetches that did not land a provider's report names.
const ENDS_KEPT: usize = 8;

#[derive(Debug)]
struct Series {
    /// Names the series to its timer: a timer outliving it must not resume
    /// the next one.
    id: u64,
    since: Instant,
    next_wait: Duration,
    /// The pause is over, and one fetch probes the provider.
    probing: bool,
}

#[derive(Debug)]
pub(super) struct Fetches {
    limit: usize,
    /// Every node registered as holding a payload, wanted or not.
    holders: HashMap<Hash, HashSet<PublicKey>>,
    wanted: HashMap<Hash, Wanted>,
    providers: HashMap<PublicKey, Provider>,
    next_series: u64,
}

impl Fetches {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            holders: HashMap::new(),
            wanted: HashMap::new(),
            providers: HashMap::new(),
            next_series: 0,
        }
    }

    /// Per provider the fetches in flight, the payloads queued and any
    /// pause, and the payloads wanted, for a failure message.
    pub(super) fn report(&self) -> String {
        let mut lines: Vec<String> = self
            .providers
            .iter()
            .filter(|(_, entry)| {
                entry.in_flight > 0 || !entry.queue.is_empty() || entry.series.is_some()
            })
            .map(|(provider, entry)| {
                let pause = match &entry.series {
                    None => "",
                    Some(series) if series.probing => ", probed",
                    Some(_) => ", paused",
                };
                let ends = if entry.ends.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", Vec::from(entry.ends.clone()).join(", "))
                };
                format!(
                    "{}: {} in flight, {} queued{pause}{ends}",
                    provider.fmt_short(),
                    entry.in_flight,
                    entry.queue.len()
                )
            })
            .collect();
        lines.sort();
        lines.push(format!("{} wanted", self.wanted.len()));
        lines.join("; ")
    }

    /// Keeps a fetch from `provider` that did not land, for the report.
    pub(super) fn noted(
        &mut self,
        provider: PublicKey,
        hash: Hash,
        outcome: Outcome,
        why: Option<String>,
    ) {
        if matches!(outcome, Outcome::Fetched | Outcome::Held) {
            return;
        }
        let Some(entry) = self.providers.get_mut(&provider) else {
            return;
        };
        if entry.ends.len() == ENDS_KEPT {
            entry.ends.pop_front();
        }
        let hash = hash.fmt_short();
        entry.ends.push_back(match why {
            Some(why) => format!("{hash} {outcome:?}: {why}"),
            None => format!("{hash} {outcome:?}"),
        });
    }

    pub(super) fn is_wanted(&self, hash: &Hash) -> bool {
        self.wanted.contains_key(hash)
    }

    /// Records `provider` as holding `hash`, and while `hash` is wanted
    /// queues it there, once per pair.
    pub(super) fn register(&mut self, hash: Hash, provider: PublicKey) -> Steps {
        let mut steps = Steps::default();
        self.holders.entry(hash).or_default().insert(provider);
        if let Some(wanted) = self.wanted.get_mut(&hash) {
            if wanted.eligible.insert(provider) {
                let entry = self.providers.entry(provider).or_default();
                entry.holds.insert(hash);
                entry.queue.push_back(hash);
                self.dispatch(provider, &mut steps);
            }
        }
        steps
    }

    /// Wants `hash`, queued at every provider registered for it; `false`
    /// when none is, and then it is not wanted here.
    #[must_use = "on `false` the caller parks the payload"]
    pub(super) fn want(&mut self, hash: Hash) -> (bool, Steps) {
        let mut steps = Steps::default();
        if self.wanted.contains_key(&hash) {
            return (true, steps);
        }
        let eligible = self.holders.get(&hash).cloned().unwrap_or_default();
        if eligible.is_empty() {
            return (false, steps);
        }
        for provider in &eligible {
            let entry = self.providers.entry(*provider).or_default();
            entry.holds.insert(hash);
            entry.queue.push_back(hash);
        }
        let providers: Vec<PublicKey> = eligible.iter().copied().collect();
        self.wanted.insert(
            hash,
            Wanted {
                eligible,
                fetching: None,
            },
        );
        for provider in providers {
            self.dispatch(provider, &mut steps);
        }
        (true, steps)
    }

    /// Stops wanting `hash`; a fetch of it in flight runs to its end.
    pub(super) fn unwant(&mut self, hash: &Hash) {
        if let Some(wanted) = self.wanted.remove(hash) {
            shrink_if_sparse!(self.wanted);
            for provider in wanted.eligible {
                if let Some(entry) = self.providers.get_mut(&provider) {
                    entry.holds.remove(hash);
                    shrink_if_sparse!(entry.holds);
                }
            }
        }
    }

    /// A fetch of `hash` from `provider` ended with `outcome`; `neighbor`
    /// says whether the provider is a gossip neighbor now.
    pub(super) fn finished(
        &mut self,
        provider: PublicKey,
        hash: Hash,
        outcome: Outcome,
        neighbor: bool,
        now: Instant,
    ) -> Steps {
        let mut steps = Steps::default();
        if let Some(entry) = self.providers.get_mut(&provider) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        if let Some(wanted) = self.wanted.get_mut(&hash) {
            if wanted.fetching == Some(provider) {
                wanted.fetching = None;
            }
        }
        match outcome {
            Outcome::Fetched => {
                self.unwant(&hash);
                if let Some(entry) = self.providers.get_mut(&provider) {
                    entry.series = None;
                }
            }
            // The provider was not asked: its series stands.
            Outcome::Held => self.unwant(&hash),
            Outcome::LocalFailure => {
                if self.wanted.contains_key(&hash) {
                    self.unwant(&hash);
                    steps.unrouted.push(hash);
                }
            }
            Outcome::Stalled => {
                self.pause(provider, now, &mut steps);
                self.requeue(hash, &mut steps);
            }
            // An answer about this payload alone: a pause would hold back
            // every payload the provider does hold.
            Outcome::Lacks => {
                self.drop_pair(hash, provider);
                self.requeue(hash, &mut steps);
            }
            Outcome::ConnectFailed | Outcome::Failed if neighbor => {
                self.pause(provider, now, &mut steps);
                self.requeue(hash, &mut steps);
            }
            Outcome::ConnectFailed => {
                self.drop_provider(provider, &mut steps);
                self.requeue(hash, &mut steps);
            }
            Outcome::Failed => {
                self.drop_pair(hash, provider);
                self.requeue(hash, &mut steps);
            }
        }
        self.dispatch(provider, &mut steps);
        self.unroute_if_stranded(hash, &mut steps);
        steps
    }

    /// The pause of `provider`'s series `series` is over: one fetch probes
    /// it.
    pub(super) fn resume(&mut self, provider: PublicKey, series: u64) -> Steps {
        let mut steps = Steps::default();
        let Some(current) = self
            .providers
            .get_mut(&provider)
            .and_then(|entry| entry.series.as_mut())
        else {
            return steps;
        };
        if current.id != series || current.probing {
            return steps;
        }
        current.probing = true;
        self.dispatch(provider, &mut steps);
        steps
    }

    /// `provider` is a gossip neighbor in no swarm any more: a series
    /// toward it ends, with its queue.
    pub(super) fn neighbor_gone(&mut self, provider: PublicKey) -> Steps {
        let mut steps = Steps::default();
        if self
            .providers
            .get(&provider)
            .is_some_and(|entry| entry.series.is_some())
        {
            self.drop_provider(provider, &mut steps);
        }
        steps
    }

    /// Starts a series of pauses toward `provider`, or arms the next pause
    /// after a failed probe; a failure while a pause runs arms nothing, so
    /// a series keeps one try due.
    fn pause(&mut self, provider: PublicKey, now: Instant, steps: &mut Steps) {
        let next_id = self.next_series;
        let entry = self.providers.entry(provider).or_default();
        let series = match &mut entry.series {
            Some(series) if !series.probing => return,
            Some(series) => {
                series.probing = false;
                series
            }
            None => {
                info!(provider = %provider.fmt_short(), "fetches toward a provider pause");
                self.next_series = next_id.wrapping_add(1);
                entry.series.insert(Series {
                    id: next_id,
                    since: now,
                    next_wait: UNREACHED_RETRY_FIRST,
                    probing: false,
                })
            }
        };
        if now.saturating_duration_since(series.since) >= UNREACHED_RETRY_FOR {
            let before = steps.unrouted.len();
            self.drop_provider(provider, steps);
            info!(
                provider = %provider.fmt_short(),
                unrouted = steps.unrouted.len().saturating_sub(before),
                "fetch tries toward a provider exhausted"
            );
            return;
        }
        let wait = series.next_wait;
        series.next_wait = wait.saturating_mul(2).min(UNREACHED_RETRY_MAX);
        steps.pause = Some((provider, series.id, wait));
    }

    /// `provider` is fetched from no more until a registration names it
    /// again; what it alone held is unrouted.
    fn drop_provider(&mut self, provider: PublicKey, steps: &mut Steps) {
        let Some(entry) = self.providers.get_mut(&provider) else {
            return;
        };
        entry.queue = VecDeque::new();
        entry.series = None;
        let holds = std::mem::take(&mut entry.holds);
        for hash in holds {
            if let Some(wanted) = self.wanted.get_mut(&hash) {
                wanted.eligible.remove(&provider);
            }
            self.unroute_if_stranded(hash, steps);
        }
    }

    fn drop_pair(&mut self, hash: Hash, provider: PublicKey) {
        if let Some(wanted) = self.wanted.get_mut(&hash) {
            wanted.eligible.remove(&provider);
        }
        if let Some(entry) = self.providers.get_mut(&provider) {
            entry.holds.remove(&hash);
            shrink_if_sparse!(entry.holds);
        }
    }

    /// Puts `hash` back at the end of every eligible provider's queue.
    fn requeue(&mut self, hash: Hash, steps: &mut Steps) {
        let Some(wanted) = self.wanted.get(&hash) else {
            return;
        };
        let eligible: Vec<PublicKey> = wanted.eligible.iter().copied().collect();
        for provider in &eligible {
            self.providers
                .entry(*provider)
                .or_default()
                .queue
                .push_back(hash);
        }
        for provider in eligible {
            self.dispatch(provider, steps);
        }
    }

    fn unroute_if_stranded(&mut self, hash: Hash, steps: &mut Steps) {
        if self
            .wanted
            .get(&hash)
            .is_some_and(|wanted| wanted.eligible.is_empty() && wanted.fetching.is_none())
        {
            self.wanted.remove(&hash);
            shrink_if_sparse!(self.wanted);
            steps.unrouted.push(hash);
        }
    }

    /// Starts fetches toward `provider` while it has room: its limit, one
    /// while it is probed, none while it pauses. A probe that finds nothing
    /// to fetch ends the series: kept, it would count the provider's next
    /// failure, however late, against its 2 minutes and drop it unpaused.
    fn dispatch(&mut self, provider: PublicKey, steps: &mut Steps) {
        let limit = self.limit;
        let Some(entry) = self.providers.get_mut(&provider) else {
            return;
        };
        let room = match &entry.series {
            None => limit,
            Some(series) if series.probing => 1,
            Some(_) => 0,
        };
        while entry.in_flight < room {
            let Some(hash) = entry.queue.pop_front() else {
                break;
            };
            let Some(wanted) = self.wanted.get_mut(&hash) else {
                continue;
            };
            if wanted.fetching.is_some() || !wanted.eligible.contains(&provider) {
                continue;
            }
            wanted.fetching = Some(provider);
            entry.in_flight = entry.in_flight.saturating_add(1);
            steps.start.push((provider, hash));
        }
        shrink_if_sparse!(entry.queue);
        if entry.in_flight == 0 && entry.series.as_ref().is_some_and(|series| series.probing) {
            entry.series = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn node(seed: u8) -> PublicKey {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn payload(seed: u8) -> Hash {
        Hash::new([seed])
    }

    /// Wants `hashes` held by `provider`, returning the fetches started.
    fn want_from(fetches: &mut Fetches, provider: PublicKey, hashes: &[Hash]) -> Vec<Hash> {
        let mut started = Vec::new();
        for hash in hashes {
            let _ = fetches.register(*hash, provider);
            let (routed, steps) = fetches.want(*hash);
            assert!(routed, "a payload with a provider is wanted");
            started.extend(steps.start.into_iter().map(|(_, hash)| hash));
        }
        started
    }

    /// A provider has its limit of fetches in flight and no more; each one
    /// that ends starts the next in its queue.
    #[test]
    fn fetches_toward_a_provider_keep_its_limit() {
        let mut fetches = Fetches::new(2);
        let a1 = node(1);
        let hashes: Vec<Hash> = (0..5).map(payload).collect();
        let started = want_from(&mut fetches, a1, &hashes);
        assert_eq!(started, hashes[..2], "fetches started at once");

        let steps = fetches.finished(a1, hashes[0], Outcome::Fetched, false, Instant::now());
        assert_eq!(steps.start, [(a1, hashes[2])]);
    }

    /// A catch-up that has ended keeps none of what its peak allocated.
    #[test]
    fn a_finished_catch_up_gives_its_capacity_back() {
        let mut fetches = Fetches::new(100);
        let a1 = node(1);
        let hashes: Vec<Hash> = (0..10_000u32).map(|i| Hash::new(i.to_le_bytes())).collect();
        let mut in_flight = want_from(&mut fetches, a1, &hashes);
        while let Some(hash) = in_flight.pop() {
            let steps = fetches.finished(a1, hash, Outcome::Fetched, false, Instant::now());
            in_flight.extend(steps.start.into_iter().map(|(_, hash)| hash));
        }
        assert_eq!(fetches.report(), "0 wanted", "every payload arrived");
        let entry = fetches.providers.get(&a1).expect("a1 has an entry");
        let capacities = [
            entry.queue.capacity(),
            entry.holds.capacity(),
            fetches.wanted.capacity(),
        ];
        assert!(
            capacities.iter().all(|capacity| *capacity < 16),
            "queue, holds and wanted capacities: {capacities:?}"
        );
    }

    /// A pair of payload and provider registered twice is queued once.
    #[test]
    fn a_pair_registered_twice_is_queued_once() {
        let mut fetches = Fetches::new(1);
        let (a1, b1) = (node(1), node(2));
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, a1, &[p, q]);
        let _ = fetches.register(q, b1);
        let _ = fetches.register(q, b1);
        let queued = fetches.providers.get(&b1).map(|entry| entry.queue.clone());
        assert_eq!(queued, Some(VecDeque::new()), "q went to b1 at once");
        let entry = fetches.providers.get(&b1).map(|entry| entry.in_flight);
        assert_eq!(entry, Some(1));
    }

    /// A payload in flight toward one provider is not fetched from a second
    /// one registered meanwhile, and goes to the second when the first
    /// stalls.
    #[test]
    fn a_stalled_payload_goes_to_another_provider() {
        let mut fetches = Fetches::new(100);
        let (a1, b1) = (node(1), node(2));
        let p = payload(1);
        want_from(&mut fetches, a1, &[p]);
        let steps = fetches.register(p, b1);
        assert_eq!(steps.start, [], "p is in flight toward a1");

        let steps = fetches.finished(a1, p, Outcome::Stalled, false, Instant::now());
        assert_eq!(steps.start, [(b1, p)]);
        assert_eq!(
            steps.pause.map(|(provider, _, wait)| (provider, wait)),
            Some((a1, UNREACHED_RETRY_FIRST))
        );
    }

    /// The only provider of a payload stalls, pauses, is probed with the
    /// payload, pauses twice as long when the probe stalls, and gets its
    /// whole limit back when a probe succeeds.
    #[test]
    fn the_only_provider_is_probed_after_each_pause() {
        let mut fetches = Fetches::new(100);
        let b1 = node(1);
        let hashes: Vec<Hash> = (0..3).map(payload).collect();
        want_from(&mut fetches, b1, &hashes);
        let t0 = Instant::now();

        let steps = fetches.finished(b1, hashes[0], Outcome::Stalled, false, t0);
        let (_, series, wait) = steps.pause.expect("a pause");
        assert_eq!(wait, Duration::from_secs(1));
        assert_eq!(steps.unrouted, [], "the only provider keeps the payload");
        // The other two stall during the pause: no second pause is armed.
        for hash in &hashes[1..] {
            let steps = fetches.finished(b1, *hash, Outcome::Stalled, false, t0);
            assert!(steps.pause.is_none(), "a pause runs already");
            assert_eq!(steps.start, [], "nothing goes to a paused provider");
        }

        let steps = fetches.resume(b1, series);
        assert_eq!(steps.start.len(), 1, "one probe");
        let (_, probe) = steps.start[0];
        let later = t0 + Duration::from_secs(11);
        let steps = fetches.finished(b1, probe, Outcome::Stalled, false, later);
        let (_, series, wait) = steps.pause.expect("the next pause");
        assert_eq!(wait, Duration::from_secs(2));

        let steps = fetches.resume(b1, series);
        let (_, probe) = steps.start[0];
        let steps = fetches.finished(b1, probe, Outcome::Fetched, false, later);
        assert_eq!(
            steps.start.len(),
            2,
            "the whole limit is back: {:?}",
            steps.start
        );
    }

    /// A timer of a series that has ended resumes nothing, the provider's
    /// next series included.
    #[test]
    fn a_stale_timer_resumes_nothing() {
        let mut fetches = Fetches::new(100);
        let b1 = node(1);
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, b1, &[p, q]);
        let steps = fetches.finished(b1, p, Outcome::Stalled, false, Instant::now());
        let (_, stale, _) = steps.pause.expect("a pause");
        let steps = fetches.finished(b1, q, Outcome::Fetched, false, Instant::now());
        assert_eq!(steps.start, [(b1, p)], "the success ended the series");
        let steps = fetches.finished(b1, p, Outcome::Stalled, false, Instant::now());
        let (_, current, _) = steps.pause.expect("a second pause");
        assert_ne!(stale, current);

        assert_eq!(fetches.resume(b1, stale).start, [], "the stale timer");
        assert_eq!(
            fetches.resume(b1, current).start,
            [(b1, p)],
            "the current timer probes"
        );
    }

    /// A provider whose series has run 2 minutes is asked for nothing, and
    /// what it alone held is unrouted, until a registration names it again.
    #[test]
    fn a_provider_silent_for_two_minutes_waits_for_a_registration() {
        let mut fetches = Fetches::new(100);
        let b1 = node(1);
        let p = payload(1);
        want_from(&mut fetches, b1, &[p]);
        let t0 = Instant::now();
        let mut steps = fetches.finished(b1, p, Outcome::Stalled, false, t0);
        let mut at = t0;
        let mut last_series = None;
        while let Some((_, series, wait)) = steps.pause {
            last_series = Some(series);
            at += wait;
            let probe = fetches.resume(b1, series);
            assert_eq!(probe.start, [(b1, p)]);
            at += NO_PROGRESS;
            steps = fetches.finished(b1, p, Outcome::Stalled, false, at);
        }
        assert!(at - t0 >= UNREACHED_RETRY_FOR, "the series ran its span");
        assert_eq!(steps.unrouted, [p]);
        assert!(!fetches.is_wanted(&p));
        let last_series = last_series.expect("the series paused");
        assert_eq!(
            fetches.resume(b1, last_series).start,
            [],
            "nothing goes to it"
        );

        // A session's end registers it again: the payload is wanted anew.
        assert_eq!(fetches.register(p, b1).start, []);
        let (routed, steps) = fetches.want(p);
        assert!(routed);
        assert_eq!(steps.start, [(b1, p)]);
    }

    /// A provider whose pause ends with nothing left to probe starts afresh:
    /// its next stall, an hour later, pauses it again rather than dropping it.
    ///
    /// The stalled payload goes to a second provider, which delivers it
    /// before the pause ends.
    #[test]
    fn a_probe_with_nothing_to_fetch_ends_the_series() {
        let mut fetches = Fetches::new(100);
        let (a1, b1) = (node(1), node(2));
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, b1, &[p]);
        let _ = fetches.register(p, a1);
        let t0 = Instant::now();
        let steps = fetches.finished(b1, p, Outcome::Stalled, false, t0);
        let (_, series, _) = steps.pause.expect("a pause");
        assert_eq!(steps.start, [(a1, p)], "a1 takes the stalled payload");
        let _ = fetches.finished(a1, p, Outcome::Fetched, false, t0);
        assert_eq!(fetches.resume(b1, series).start, [], "nothing to probe");

        want_from(&mut fetches, b1, &[q]);
        let later = t0 + Duration::from_secs(3_600);
        let steps = fetches.finished(b1, q, Outcome::Stalled, false, later);
        assert_eq!(steps.unrouted, [], "b1 still holds q");
        assert_eq!(
            steps.pause.map(|(_, _, wait)| wait),
            Some(UNREACHED_RETRY_FIRST),
            "a new series"
        );
    }

    /// A connect failure toward a provider that is no gossip neighbor drops
    /// its queue; toward a neighbor it pauses the provider.
    #[test]
    fn a_connect_failure_drops_a_provider_unless_it_is_a_neighbor() {
        let (a1, b1) = (node(1), node(2));
        let (p, q) = (payload(1), payload(2));

        let mut fetches = Fetches::new(1);
        want_from(&mut fetches, a1, &[p, q]);
        let steps = fetches.finished(a1, p, Outcome::ConnectFailed, false, Instant::now());
        assert!(steps.pause.is_none());
        assert_eq!(
            steps.unrouted.len(),
            2,
            "both held by a1 alone: {:?}",
            steps.unrouted
        );

        let mut fetches = Fetches::new(1);
        want_from(&mut fetches, b1, &[p, q]);
        let steps = fetches.finished(b1, p, Outcome::ConnectFailed, true, Instant::now());
        assert!(steps.pause.is_some(), "a neighbor pauses");
        assert_eq!(steps.unrouted, []);
    }

    /// A provider that fails a request once connected, and is no gossip
    /// neighbor, loses that payload alone.
    #[test]
    fn a_failed_request_drops_its_pair_alone() {
        let mut fetches = Fetches::new(1);
        let a1 = node(1);
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, a1, &[p, q]);
        let steps = fetches.finished(a1, p, Outcome::Failed, false, Instant::now());
        assert_eq!(steps.unrouted, [p]);
        assert_eq!(steps.start, [(a1, q)]);
    }

    /// A provider that answers that it lacks a payload loses that payload
    /// alone and keeps fetching the rest, though it is a gossip neighbor.
    #[test]
    fn a_provider_lacking_a_payload_keeps_fetching_the_rest() {
        let mut fetches = Fetches::new(1);
        let a1 = node(1);
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, a1, &[p, q]);
        let steps = fetches.finished(a1, p, Outcome::Lacks, true, Instant::now());
        assert!(steps.pause.is_none());
        assert_eq!(steps.unrouted, [p]);
        assert_eq!(steps.start, [(a1, q)], "a1 keeps its limit");
    }

    /// A fetch failing on the node's own blob store unroutes its payload and
    /// pauses no provider.
    ///
    /// The provider is a gossip neighbor, the one a failure of its own would
    /// pause.
    #[test]
    fn a_local_failure_pauses_no_provider() {
        let mut fetches = Fetches::new(1);
        let a1 = node(1);
        let (p, q) = (payload(1), payload(2));
        want_from(&mut fetches, a1, &[p, q]);
        let steps = fetches.finished(a1, p, Outcome::LocalFailure, true, Instant::now());
        assert!(steps.pause.is_none());
        assert_eq!(steps.unrouted, [p]);
        assert_eq!(steps.start, [(a1, q)], "a1 keeps its limit");
    }

    /// A neighbor that leaves while its series runs is asked for nothing
    /// more.
    #[test]
    fn a_neighbor_gone_during_its_series_is_dropped() {
        let mut fetches = Fetches::new(1);
        let b1 = node(1);
        let p = payload(1);
        want_from(&mut fetches, b1, &[p]);
        let _ = fetches.finished(b1, p, Outcome::ConnectFailed, true, Instant::now());
        let steps = fetches.neighbor_gone(b1);
        assert_eq!(steps.unrouted, [p]);
    }
}
