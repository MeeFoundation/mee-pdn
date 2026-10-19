//! The endpoint's path selector: iroh's default choice by tier and biased
//! RTT, except that a path no connection receives on gives way to one some
//! connection does, and within a tier a path fewer connections receive on
//! gives way to one more of them receive on. Why iroh's own selector is
//! replaced, and what this one answers: the `node-assembly` spec, "A peer
//! that restarts at a new address is reached at it".

use std::cmp::Reverse;
use std::collections::HashMap;
use std::hash::Hash;

use iroh::endpoint::transports::{
    AddrKind, PathSelection, PathSelectionContext, PathSelectionData, PathSelector,
};

/// iroh's preference for IPv6 over IPv4, in nanoseconds of RTT.
const IPV6_RTT_ADVANTAGE_NANOS: i128 = 3_000_000;

/// iroh's stickiness: a path of the same tier wins only this much faster.
const RTT_SWITCHING_MIN_NANOS: i128 = 5_000_000;

/// A primary path is used whenever one is available; a relay is a backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Primary,
    Backup,
}

/// What the selector reads of one path on one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    tier: Tier,
    biased_rtt: i128,
    /// The connection has received datagrams on this path.
    received: bool,
}

impl Observed {
    fn key(self) -> (Tier, i128) {
        (self.tier, self.biased_rtt)
    }
}

/// The selector `bind_endpoint` installs on every endpoint of a node.
#[derive(Debug, Default)]
pub(crate) struct ReceivingPathSelector;

impl PathSelector for ReceivingPathSelector {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let observed: Vec<(PathSelectionData<'_>, Observed)> = ctx
            .paths()
            .filter_map(|data| {
                let stats = data.stats()?;
                let (tier, bias) = match data.network_path().addr_kind() {
                    AddrKind::IpV6 => (Tier::Primary, -IPV6_RTT_ADVANTAGE_NANOS),
                    AddrKind::Relay => (Tier::Backup, 0),
                    _ => (Tier::Primary, 0),
                };
                let rtt = i128::try_from(stats.rtt.as_nanos()).unwrap_or(i128::MAX);
                let observed = Observed {
                    tier,
                    biased_rtt: rtt.saturating_add(bias),
                    received: stats.udp_rx.datagrams > 0,
                };
                Some((data, observed))
            })
            .collect();
        let paths: Vec<_> = observed
            .iter()
            .map(|(data, observed)| (data.network_path(), *observed))
            .collect();
        let mut selection = PathSelection::none();
        if let Some((data, _)) = choose(ctx.current(), &paths).and_then(|at| observed.get(at)) {
            selection.set(data);
        }
        selection
    }
}

/// What the selector holds of one path across the connections carrying it.
#[derive(Debug, Clone, Copy)]
struct Seen {
    /// The entry with the path's lowest key, as iroh reads a path several
    /// connections carry.
    at: usize,
    key: (Tier, i128),
    receiving: usize,
}

impl Seen {
    /// Lower is preferred: received on by some connection, then tier, then
    /// received on by more connections. Biased RTT decides only between
    /// paths alike in these, with iroh's stickiness.
    fn standing(self) -> (bool, Tier, Reverse<usize>) {
        (self.receiving == 0, self.key.0, Reverse(self.receiving))
    }
}

/// The index of the path to select among `paths`, one per path of a
/// connection, or `None` to keep `current`. Choosing again from the path
/// chosen keeps it.
fn choose<P: Eq + Hash>(current: Option<&P>, paths: &[(&P, Observed)]) -> Option<usize> {
    let mut seen: HashMap<&P, Seen> = HashMap::with_capacity(paths.len());
    for (at, (path, observed)) in paths.iter().enumerate() {
        let path_seen = seen.entry(*path).or_insert(Seen {
            at,
            key: observed.key(),
            receiving: 0,
        });
        if observed.key() < path_seen.key {
            path_seen.at = at;
            path_seen.key = observed.key();
        }
        if observed.received {
            path_seen.receiving = path_seen.receiving.saturating_add(1);
        }
    }
    let best = *seen
        .values()
        .min_by_key(|path_seen| (path_seen.standing(), path_seen.key.1, path_seen.at))?;
    let Some(current) = current.and_then(|current| seen.get(current)).copied() else {
        return Some(best.at);
    };
    if best.at == current.at {
        return None;
    }
    let switch = best.standing() < current.standing()
        || best.key.1.saturating_add(RTT_SWITCHING_MIN_NANOS) <= current.key.1;
    switch.then_some(best.at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(rtt_micros: i128, received: bool) -> Observed {
        Observed {
            tier: Tier::Primary,
            biased_rtt: rtt_micros * 1_000,
            received,
        }
    }

    fn relay(rtt_micros: i128) -> Observed {
        Observed {
            tier: Tier::Backup,
            biased_rtt: rtt_micros * 1_000,
            received: true,
        }
    }

    /// A peer killed and started again on a new port: the old path keeps the
    /// one connection that received on it before the kill, and its last
    /// RTT; the new path carries the two connections the restarted peer
    /// opened. The new path wins although it is less than 5 ms faster.
    #[test]
    fn a_path_more_connections_receive_on_replaces_the_current_one() {
        let (old, new) = ("old", "new");
        let paths = [
            (&old, direct(1_929, true)),
            (&new, direct(392, true)),
            (&new, direct(572, true)),
        ];
        assert_eq!(choose(Some(&old), &paths), Some(1));
    }

    /// The old path also carries dials that never finished their handshake:
    /// they received nothing, and the path counts as the one connection
    /// that did.
    #[test]
    fn a_dial_that_received_nothing_does_not_count_for_its_path() {
        let (old, new) = ("old", "new");
        let paths = [
            (&old, direct(1_929, true)),
            (&old, direct(333_000, false)),
            (&old, direct(333_000, false)),
            (&new, direct(392, true)),
            (&new, direct(572, true)),
        ];
        assert_eq!(choose(Some(&old), &paths), Some(3));
    }

    /// Once the last connection that received on the current path closes,
    /// one connection receiving elsewhere is enough. The dead path keeps its
    /// last RTT, under 5 ms slower, so iroh's stickiness alone would keep it.
    #[test]
    fn a_current_path_no_connection_receives_on_gives_way_to_any_that_one_does() {
        let (old, new) = ("old", "new");
        let paths = [(&old, direct(1_929, false)), (&new, direct(392, true))];
        assert_eq!(choose(Some(&old), &paths), Some(1));
    }

    /// As many connections receiving on each path: iroh's stickiness
    /// decides, keeping the current path against one under 5 ms faster and
    /// leaving it for one at least that much faster.
    #[test]
    fn paths_received_on_alike_are_chosen_by_iroh_stickiness() {
        let (current, other) = ("current", "other");
        let slightly = [
            (&current, direct(5_000, true)),
            (&other, direct(1_000, true)),
        ];
        assert_eq!(choose(Some(&current), &slightly), None);
        let clearly = [
            (&current, direct(6_000, true)),
            (&other, direct(1_000, true)),
        ];
        assert_eq!(choose(Some(&current), &clearly), Some(1));
    }

    /// A path fewer connections receive on does not take over, however much
    /// faster.
    #[test]
    fn a_path_fewer_connections_receive_on_does_not_take_over() {
        let (current, other) = ("current", "other");
        let paths = [
            (&current, direct(9_000, true)),
            (&current, direct(9_000, true)),
            (&other, direct(1_000, true)),
        ];
        assert_eq!(choose(Some(&current), &paths), None);
    }

    /// A peer started again on a slower network: the new path, which more
    /// connections receive on, is chosen and kept although the dead one
    /// kept a lower RTT.
    #[test]
    fn a_slower_path_more_connections_receive_on_is_kept() {
        let (old, new) = ("old", "new");
        let paths = [
            (&old, direct(1_929, true)),
            (&new, direct(40_000, true)),
            (&new, direct(41_000, true)),
        ];
        assert_eq!(choose(Some(&old), &paths), Some(1));
        assert_eq!(choose(Some(&new), &paths), None);
    }

    /// A relay every connection receives on does not displace a direct path
    /// fewer of them receive on yet, and a selected relay gives way to it.
    #[test]
    fn a_relay_more_connections_receive_on_yields_to_a_direct_path() {
        let (relayed, direct_path) = ("relay", "direct");
        let paths = [
            (&direct_path, direct(2_000, true)),
            (&relayed, relay(40_000)),
            (&relayed, relay(41_000)),
            (&relayed, relay(42_000)),
        ];
        assert_eq!(choose(Some(&direct_path), &paths), None);
        assert_eq!(choose(Some(&relayed), &paths), Some(0));
    }

    /// A direct path only failed dials carry loses to a relay that receives:
    /// the dead port is not chosen for its tier.
    #[test]
    fn a_direct_path_no_connection_receives_on_yields_to_a_relay() {
        let (relayed, dead) = ("relay", "dead");
        let paths = [
            (&dead, direct(333_000, false)),
            (&dead, direct(1_929, false)),
            (&relayed, relay(50_000)),
            (&relayed, relay(50_000)),
        ];
        assert_eq!(choose(Some(&relayed), &paths), None);
        assert_eq!(choose(Some(&dead), &paths), Some(2));
    }

    /// Whatever path is current, choosing once more from the path chosen
    /// keeps it.
    #[test]
    fn the_path_chosen_is_kept_when_chosen_again() {
        let (a, b, c) = ("a", "b", "c");
        let sets: [&[(&&str, Observed)]; 4] = [
            &[
                (&a, direct(1_929, true)),
                (&b, direct(40_000, true)),
                (&b, direct(41_000, true)),
            ],
            &[
                (&a, direct(2_000, true)),
                (&c, relay(30_000)),
                (&c, relay(30_000)),
                (&c, relay(30_000)),
            ],
            &[
                (&a, direct(1_929, false)),
                (&b, direct(392, true)),
                (&c, relay(5_000)),
            ],
            &[
                (&a, direct(5_000, true)),
                (&b, direct(1_000, true)),
                (&c, direct(300, false)),
            ],
        ];
        for paths in sets {
            for current in [None, Some(&a), Some(&b), Some(&c)] {
                let Some(at) = choose(current, paths) else {
                    continue;
                };
                let chosen = paths[at].0;
                assert_eq!(
                    choose(Some(chosen), paths),
                    None,
                    "{paths:?} from {current:?}"
                );
            }
        }
    }

    /// A direct path replaces a relay at once when connections receive on
    /// both alike, as iroh's tiers have it.
    #[test]
    fn a_direct_path_replaces_a_relay() {
        let (relayed, direct_path) = ("relay", "direct");
        let paths = [
            (&relayed, relay(5_000)),
            (&direct_path, direct(80_000, true)),
        ];
        assert_eq!(choose(Some(&relayed), &paths), Some(1));
    }

    /// With no current path, the best path by tier and biased RTT is chosen;
    /// with no path at all, nothing is.
    #[test]
    fn without_a_current_path_the_best_one_is_chosen() {
        let (slow, fast) = ("slow", "fast");
        let paths = [(&slow, direct(9_000, true)), (&fast, direct(1_000, true))];
        assert_eq!(choose(None, &paths), Some(1));
        let none: [(&&str, Observed); 0] = [];
        assert_eq!(choose(None, &none), None);
    }
}
