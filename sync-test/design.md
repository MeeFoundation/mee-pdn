# Sync reliability test: one phone in a pod

Design draft, 2026-10-07. It replaces the grant-based draft kept in `grants-era/`: data now moves between people through a pod, so the test measures a pod.

## Summary

One command, one phone, ten laptop nodes, all members of one pod: the test has every laptop member place 1,000 numbered records into the pod over 100 seconds, cuts the phone's network for 30 seconds in the middle, and ends with PASS, FAIL or INCONCLUSIVE plus latency percentiles. The only manual step is having the phone on a cable, unlocked, on the same Wi-Fi.

The scenario in brief:

1. Writer w1 creates the pod. Writers w2..w10, a control member `k` and the phone join it on invites w1 mints.
2. Every member places one warm-up record. Measurement starts once the phone and the laptop reader list every warm-up record, so every member's membership is known everywhere before the first measured write.
3. Each writer places one numbered immutable-document every 100 ms for 100 seconds, and every 5 seconds appends one operation to a shared mergeable-document w1 placed at setup.
4. At 20 seconds w1 removes `k`. From then on `k` is the paired denial: a former member holding both stores' tickets.
5. From 40 to 70 seconds a firewall rule cuts the phone off from the laptop; the test's own control channel stays up. The laptop members keep syncing among themselves.
6. Every node reports each entry it receives and each sync session it finishes, per store.
7. Writing stops; the test waits until the phone has had a qualifying session for every expected entry, then lists what the phone holds, with each entry's verdict.
8. Every entry is classified, and the verdict and the numbers are printed.

The key rule, by example. Entry 417 of w3 is written at 41.203, during the cut. The laptop members pull it from w3 within a second. At 70.000 the cut lifts.

- At 71.000 the phone lacks it, and no session of the phone's has yet started with a member that held it: in flight, which is normal.
- A record-store session between the phone and w5 started at 71.300 on w5's side, after w5 received 417 at 41.900, and ended ok on both sides. If 417 has not arrived on the phone before the phone's side of that session ended, it is violated: a defect, however long we wait.
- If the cut never lifts, no session ever qualifies, and the entry is stalled: the run is INCONCLUSIVE, neither a pass nor a proven loss.

What a report looks like (the format, not a measurement):

```
VERDICT: FAIL — 1 violated, 1 lost
expected 10,560 · readable 10,559 · stalled 0 · split sessions 0 · undetermined 0 · uncounted 0
latency, steady, phone: p50 0.9 s · p90 1.6 s · p99 2.4 s (clock ±4 ms)
recovery, phone: first session 0.8 s · catch-up 3.1 s · converge 2.2 s
LOST w3 by/<w3>/immutable-document/<id>/1 -> phone: t_write 41.203; w5 accepted 71.300-71.420 ok, phone dialed 71.290-71.480 ok (phone clock)
```

## What the test answers

Does every entry placed into a pod reach the phone, does it read there once it has arrived, and how long does that take. It separates an entry that has not arrived yet from one that sync has already failed to deliver, and it says so without a waiting timer.

Per run:

- how many entries were delivered, lost, corrupted, left uncounted, or received by the removaled member where no member should have served them;
- how long an entry takes from its placement to being readable on the phone, as percentiles;
- how long the phone takes to catch up after its network is cut and restored, and whether it catches up from members that are not the author;
- whether the run itself can be trusted: host CPU, event loss, clock error, network path.

It deliberately does not answer:

- relay and NAT traversal performance: the main run uses a direct Wi-Fi path; a relay run is a separate variant.
- battery and radio cost: the phone stays in the foreground on the agent screen.
- what a merge of a mergeable-document's operations produces: merging happens above the platform; the test checks that every operation arrives and reads with its writer.
- a member's device that contradicts its own member's history: the pods change takes a member's own history on its word, and the test plants no such forgery.
- throughput limits of one laptop: when the host CPU saturates, latency is reported as the machine's limit.

## Setup

One laptop runs every node and the coordinator; one physical iPhone runs a test build of the application; both sit on one Wi-Fi network. The phone is connected by cable, unlocked, auto-lock off, on the same Wi-Fi, and the test build is installed once with Local Network access allowed. Runs reset the node's storage instead of reinstalling, since a reinstall asks for that permission again.

Entries between the phone and every laptop node travel as iroh QUIC over direct UDP through the router. The shaper touches only that UDP; the coordinator reaches the agent over TCP, so cutting the phone's data path never cuts the test's control of it. Laptop nodes run `PDN_CONNECTIVITY=direct`, so the only path from the phone to them is direct UDP, and pf rules dropping UDP to and from the phone's address are a real partition. Delay and loss use the same rules through dummynet.

| Participant | Where | Role in the main run |
|---|---|---|
| Writers w1..w10 | laptop, one `pdn-node-http` each | Members; w1 is the creator and the one owner; each places records and appends operations |
| Laptop reader `r` | laptop node | A member that writes nothing; the phone's stand-in for laptop-only runs, and a second reader in phone runs |
| Phone | iPhone, `pdn-mobile` node inside the test build | A member that writes nothing; the reader being measured |
| Control `k` | laptop node | Joins, places its warm-up record and 10 records, and is removed at 20 seconds |
| Coordinator | laptop, one script | Runs the scenario, keeps the journal, subscribes to every node's events, computes the verdict |
| Agent | inside the test build | Executes the coordinator's commands through the same facade the screens use, and forwards the phone node's events |

Every node hosts one identity, so a session's peer node names its member.

The control exists because a test that only proves authorized parties receive data verifies nothing about access (`access-control-tests.md`). Inside a pod there is no narrower audience than the members, so the tightest unauthorized party is a former member: `k` holds both stores' write tickets and a replica of each, exactly as a member does, and lacks only the membership. No hand-made ticket handover is needed or admitted: `k` reaches the pod on the product path, through an invite and a join, and leaves it through an owner's remove. An outsider that never joined is covered by `crates/data-layer/tests/pod_forgeries.rs` and `pod_sessions.rs`, because the HTTP host hands over no store ticket.

## How a pod moves entries

Everything the classification stands on, with where it lives in the code:

- **Both stores are whole replicas held by every member device**, each in a gossip swarm of its own (`create_pod`, `import_pod_with` in `crates/data-layer/src/node.rs`). A served member gets the whole store with no egress filter (`serve_pod` in `access.rs`), so an entry reaches the phone from whichever member device holds it — the author or any relay.
- **Gossip carries no entries.** A local write broadcasts the author's head to direct neighbours; a neighbour with news dials the sender and pulls; a puller that received anything re-broadcasts to its own neighbours (`on_replica_event`, `on_sync_report`, `on_sync_finished` in `crates/pdn-store/src/engine/live.rs`). The steady path is therefore a cascade of pulls, one hop per session.
- **The safety net is the pod pass**: every `pod_reconcile_interval` (300 seconds by default) a node syncs each pod store with at most 5 members' devices drawn at random (`pod_reconcile_pass` in `node.rs`). The node's ordinary reconcile interval does not reach pod stores between nodes. The run sets the pod interval explicitly on every node and records it, because the stall deadline is sized from it.
- **The membership store syncs before the record store**: a record-store session with a peer waits for a membership-store session with the same peer (`order_pod_stores`, `held_dials`). Every pull of a record is two sessions.
- **A session serves a view of the replica frozen when it is set up**, and reconciliation is symmetric: a successful session leaves both sides holding the union of their views, whichever side dialed. Both origins carry entries, unlike a grant-imported replica, whose holder only pulls.
- **Arrival is not reading.** A record entry counts only once the membership fold knows its writer at the membership sequence the entry names, the entry's author is among that writer's device statements, and its payload has arrived (`judge` in `crates/data-layer/src/pod/record_view.rs`). A present entry can be `NotYet` or counted for nothing until the membership it needs arrives. The warm-up phase makes every writer's membership known before measurement, so a measured entry reads as soon as its payload is there, and an entry present and uncounted at the final listing is a defect.
- **No pod read nudges a sync** (`list_records`, `read`, `pod_record_view` start no session), so the final listing and any poll observe without driving what they measure.
- **A removed device is refused the record store** by every member whose fold holds the removal, from that member's next session (`serve_pod`), and forgets its record store once it folds the removal itself (`settle_departure`, `forget_pod`). A member that has not yet folded the removal may still serve `k`; that is the spec's eventual rule, not a defect.

## The run, step by step

```
sync-test --writers 10 --records 1000 --rate 10 --payload 256 --pod-interval 30 --scenario partition
```

1. **Preflight.** The phone is attached (`xcrun devicectl list devices`); the git revisions, build profile and core count are recorded.
2. **Bring up.** Fresh directories for every laptop node; 10 writers, `r` and `k` start from a release build with `PDN_POD_RECONCILE_INTERVAL_SECS` set.
3. **Launch the agent** over the cable with the coordinator's address and the run id as launch environment. The agent deletes its node's directory, brings up a fresh node on the same pod interval, creates one identity, and reports the phone's Wi-Fi address.
4. **Network check.** pf rules for UDP to and from the phone with no delay and no loss; a UDP probe; abort unless the rules' counters rose.
5. **Clock sample.** 20 timestamped pings, the one with the smallest round trip kept; repeated every 30 seconds.
6. **Form the pod.** w1 creates it and places the shared mergeable-document. Each other member joins on its own invite from w1; the phone's join fails loudly if the phone cannot reach w1. Every member places one warm-up record. The coordinator waits until `r` and the phone list every warm-up record and the mergeable-document.
7. **Subscribe.** The coordinator opens the event route on every laptop node, for both stores; the agent forwards the phone's.
8. **Steady (0–40 s).** Each writer places its next record every 100 ms, and every 5 seconds appends an operation to the shared document. `k` places 10 records in the first 10 seconds. At 20 seconds w1 removes `k`.
9. **Partition (40–70 s).** pf drops the phone's UDP. Writers keep writing; the laptop members keep syncing among themselves.
10. **Recovery (70–100 s).** pf passes traffic again; writing continues to the end.
11. **Quiescence.** Writing stops. The coordinator waits until every expected entry has a qualifying session on every reader (below), or until the stall deadline: 5 × the pod interval after the last write.
12. **Final listing.** The agent uploads the phone's record view: every record-store entry — one per key and author — with its hash, length, payload presence, payload and verdict. `r` is listed the same way. The agent's local journal is pulled over the cable.
13. **Verdict.** Every expected entry is classified for every reader, `k` is checked against its rule, the metrics are computed, one row is appended to `runs.csv`, and the verdict is printed.

Other scenarios change only steps 8–10:

| Scenario | Change | Question it answers |
|---|---|---|
| steady | no partition | Baseline latency and loss |
| partition | as above | Catch-up time; zero loss across a cut |
| author offline | as partition, and w3 is stopped at 60 s and stays stopped | The phone catches up w3's records from members that are not their author |
| lossy | 50 ms one-way delay and 2% loss for the whole run | Behaviour on a bad link |
| restart | the app is killed and relaunched at 40 s; the agent brings the node up on the same directory | The pod is re-armed from the directory, and catch-up resumes |
| join under load | the phone joins at 50 s, while the writers write | A newcomer's first sessions bring the whole history; expected set unchanged |
| fan-out | the phone writes, the laptop reads; journal times are the phone's clock | Load on the sending side |
| same identity | the phone is a second device of `r`'s identity, linked, not joined | A member's other device reaches the pod through its PMS (`arm_pods`) |
| leave | at 60 s `r` leaves | Nothing written after the leave reaches `r`; what `r` placed stays readable on the phone |

## Test data and what is recorded

**Records.** Each writer places immutable-documents with sequence numbers 1, 2, …; each record is one entry under a fresh key `by/<member>/immutable-document/<id>/<mseq>` (`RecordKey` in `crates/data-layer/src/pod/keys.rs`). Nothing is replaced, so every placement is its own unit of measurement and no version is ever superseded. The operations on the shared document are entries too, each under its own key ending `<writer>.<author>.<mseq>.<op_seq>`; every one is expected.

**Payload.** Self-describing and self-checking, padded to `--payload` bytes:

```
{"run":"20261007-1412","writer":"w3","seq":417,"pad":"xxxx…","sha256":"9f2c…"}
```

`sha256` covers the other fields, so a reader proves an entry intact without asking the writer.

**Journal.** One line once the call has answered: writer, seq, `kind` (`record` or `op`), the entry's `key` and `hash` and `t_write`, copied from the writer's `insert_local` event, the payload's `sha256`, and `result` (`ok` or `refused` with a reason). The coordinator issues one writer's calls one after another, and the runtime writes a record or an operation as one entry under its state lock, so a writer's record-store `insert_local` events follow its journaled calls one to one. `t_write` is the entry's own timestamp, set at insert on the writer's clock (`Record::new_current` in `crates/pdn-store/src/sync.rs`). A refused write is expected nowhere and counted on its own line.

**Event log.** Every node reports, per store, from the sync engine's own stream (`LiveEvent` in `crates/pdn-store/src/engine.rs`), timestamped where it is produced:

| Event | Recorded fields | Used for |
|---|---|---|
| `insert_local` | store, key, entry timestamp, hash, length | The journal's key, hash and `t_write`; a writer holding its own entry |
| `insert_remote` | store, key, entry timestamp, hash, length, sending node, content status | Arrival on a reader; a relay holding an entry |
| `content_ready` | store, hash | The payload is downloaded |
| `pending_content_ready` | store | The downloads queued by the last session have completed or failed |
| `sync_finished` | store, peer node, origin, session id, started, finished, ok, error and its stage, entries sent and received | Which sessions happened, how they ended, and how many entries each carried |
| `lagged` | store | The engine's stream dropped events |
| `closed` | store | The subscription ended because the store was forgotten (the removaled member's record store) |

The runtime numbers its events once, and later hops pass them through unnumbered; a gap in the numbers is treated like `lagged`. The session id is derived by both sides from the connection's keying material, so the two sides of one session pair up whatever else happened on either.

**Clock samples.** As before: the sample with the smallest round trip gives the phone's clock offset and its error bound. The offset is used for latency only.

## Classifying every entry

An absent entry becomes a failure only when a session that must have carried it has ended successfully on both sides and the entry is still not on the reader. Elapsed time alone never turns "not yet" into "lost", and no decision compares two devices' clocks.

**When a member device holds an entry.** The author holds it from its `t_write` (its own clock). Any other member device P holds it from the time of P's first `insert_remote` of its key (P's clock). An observed insert time is never earlier than the insert itself, so a session judged against it can only be judged too late, never too early.

**The qualifying session** for entry e on reader R is a record-store session between R and any member device P, of either origin, paired by session id, in which:

- both sides report ok;
- R's received count equals P's sent count;
- P's side started after P held e, both on P's clock. A session's `started` is taken before its view is frozen, so the view holds e.

**Violated:** e has no `insert_remote` on R before R's `sync_finished` of its first qualifying session, in R's stream order. **Lost:** e is not readable at the final listing although a qualifying session exists.

**Untrusted sessions.** From the first lost event on either side's stream (a `lagged`, a numbering gap, a new subscription) and from the first session whose counts disagree, that pair of nodes' sessions are not used, and an entry with no trusted qualifying session is undetermined rather than stalled. P's knowledge of an entry is untrusted if P's stream lost events before P's first insert of it.

All states, per pair of an expected entry and a reader:

| State | Condition | Meaning |
|---|---|---|
| readable | present, counted, payload hash matches the journal, payload self-check passes | Delivered; latency is computed |
| in flight | absent, no qualifying session yet | Normal; its age is reported |
| violated | absent when R's side of a qualifying session finished | Defect: the session delivered less than its view |
| lost | not readable at the final listing, although a qualifying session exists | Defect: a permanent loss |
| content lost | present at the final listing, payload absent | Defect: the payload never arrived |
| uncounted | present with its payload at the final listing, verdict not counted | Defect: the warm-up made its writer's membership known, so nothing should keep it from reading; the report names the verdict |
| stalled | absent, no qualifying session before the stall deadline | Loss neither shown nor excluded: INCONCLUSIVE |
| undetermined | absent, and the events that could show a qualifying session were lost | No violation is claimed without evidence |

Findings that are not per entry:

| Finding | Condition | Meaning |
|---|---|---|
| split session | one side reports ok, the other an error, other than a teardown split | Defect in session reporting |
| corrupt | a listed payload whose hash or self-check does not match | Defect: integrity |
| extra | a reader lists an entry no journal has; or `k` receives a record-store entry in a session whose serving peer held the removal before its side started | Defect: access control |

**The removed member's rule.** For every `insert_remote` in `k`'s record store, the carrying session is `k`'s next `sync_finished` with the sending node in `k`'s stream order. If the sending node P held the removal event — its first insert of the removal's key in the membership store — before P's side of that session started, P served the record store to a member P knew was gone: extra. A session with a member that had not yet folded the removal proves nothing either way. `k`'s record-store stream ends in `closed` once `k` folds its own departure; a stream that never closes is reported, not failed.

**Records stay.** The 10 records `k` placed before the removal are expected on every reader like any other; a removed member's records reading nowhere would break the pods invariant that what may be done to a record does not depend on whether its member is still a member.

## Metrics

Every number is computed from the journal, the event logs and the final listings.

**Verdict.** FAIL on any defect: violated, lost, content lost, uncounted, split, corrupt, extra. INCONCLUSIVE when there is no defect but at least one stalled entry, undetermined entries above 1% of expected, a reader with no listing, or the app leaving the foreground. PASS otherwise.

**Latency**, per readable pair: `t_readable(R) − offset(R) − t_write`, where `t_readable` is the `content_ready` of the payload's hash on R, or the `insert_remote` itself when the content was already complete. Reported as p50, p90, p99 and max with nearest-rank percentiles, per phase and per reader. The steady-state shape is a gossip cascade — one announcement, one pull per hop — so its numbers are sub-second to a few seconds on a direct path, and a long tail points at a lost announcement waiting for the pod pass.

**Clock check.** A corrected arrival must lie between `t_write` and the finish of the carrying session's serving side, both on the laptop's clock, each widened by the error bound. Laptop nodes share one clock; a phone-to-phone variant needs its own check.

**Recovery**, for a cut: time to first session (the first qualifying session after the cut ends), catch-up (the last readable time among entries written during the cut, minus the cut's end), converge (the last readable time of the run minus the last write). In the author-offline scenario, the report also counts the stopped writer's entries whose qualifying session was with a member other than their author.

**Session health**, per reader and peer, per store and per origin: successful, failed, split and one-sided sessions, their errors, the gaps between successful record-store sessions, and whether each record-store session had a membership-store session with the same peer before it.

**Validity**, printed first: host CPU below 90% of the cores, undetermined below 1%, every clock sample's half round trip below 20 ms, every phone connection direct, pf counters rising for the probe, the app never leaving the foreground. None of these ever turns a defect into a pass.

## Why the numbers can be trusted

| Threat | What would go wrong | How it is closed |
|---|---|---|
| A timer decides loss | A slow entry past a budget reads as lost | Loss is decided by a qualifying session, never by elapsed time |
| One side's word decides | One side reports ok for a session the other failed | Both sides must report ok; disagreement is a split session |
| A session is credited with what its server did not hold | A relay's session is taken as proof for an entry the relay received later | The serving side must have held the entry before its side started, on its own clock |
| Arrival taken as reading | An entry present but uncounted is reported delivered | The final listing carries each entry's verdict; present and uncounted is its own defect |
| Membership still converging inside the numbers | Early records wait for their writer's membership, and latency measures the join | Warm-up: measurement starts once every reader lists every member's warm-up record |
| The observer drives sync | Polling shortens the latency it measures | No pod read nudges a sync; events during the run, one listing at the end |
| Only present data is checked | Nothing the reader lacks is counted | Expected entries come from the journal; every one is classified |
| Clocks disagree | Phone and laptop differ by seconds | Holding is judged on the server's clock, presence by the reader's stream order; offsets enter latency only |
| Lost events | A hop drops events | Lagged and numbering gaps make entries undetermined, never violated |
| Lost reports | The agent's report of an arrival is lost | The agent's local journal is pulled over the cable |
| A vacuous control | A control that never dials trivially receives nothing | `k` holds both tickets and a replica of each, joined on the product path |
| A control judged on a race | `k` served by a member that had not folded the removal is counted as a leak | Only a server that held the removal before its side started counts |
| The shaper does nothing | A bad-network run on a perfect network | pf counters must rise for a probe, or the run aborts |
| Host saturation | The laptop sets latency | CPU recorded; latency marked host-bound |
| The phone sleeps | iOS stops the node in the background | Auto-lock off; app state reported; a run that left the foreground is invalid |
| Leftover state | A warm directory holds the last run's pod | Fresh laptop directories; the agent deletes the phone node's directory |
| The test build differs from the product | Measuring code the product does not run | The agent calls the facade the screens call; the build adds only the agent screen, the control channel, the event stream and the record view |

**The test is tested.** Before its numbers count, the harness runs against defects planted on purpose, with `r` standing in for the phone. Each must come out as exactly the stated report:

| Planted defect | How | Expected report | Tests |
|---|---|---|---|
| journaled but never written | the coordinator journals a record of w2 and skips the call | violated, then lost; FAIL | classifier |
| wrong sha | w3 places one payload with a wrong `sha256` | exactly one corrupt; FAIL | platform path |
| leak to the removaled member | the coordinator adds to `k`'s events one `insert_remote` from w1 after w1 held the removal | exactly one extra; FAIL | classifier |
| uncounted | the coordinator marks one listed entry's verdict `not_yet` | exactly one uncounted; FAIL | classifier |
| split session | the reader's side of one session is rewritten to an error on ingest | exactly one split; FAIL | classifier |
| partition never lifted | the reader stays cut to the end | stalled, none violated, none lost; INCONCLUSIVE | platform path |
| clock shifted | the reader's offset is shifted by +2 s, then −2 s | clock invalid in both; counts unchanged | classifier |
| event hop drops | a range of `seq` is dropped from one node's events on ingest | undetermined, zero violations claimed from them | classifier |
| author offline | w3 stopped at 60 s for the rest of the run | PASS; w3's later records qualify through relays | platform path |
| slow safety net | the pod interval is 120 s | PASS, longer tail, zero stalled | platform path |

**What a clean run proves.** Zero losses bounds the loss rate below 3/n at 95% confidence (the rule of three, `flaky-tests.md`). Entries fail together when a session fails, so the report gives the per-session bound (the record-store sessions the reader took part in) beside the per-entry one, which holds only if losses were independent.

**Every defect is reproducible.** The report names the writer, the reader, the key, `t_write`, both sides of the session that should have carried it, and the log directory — enough to replay the case as a scenario test in the workspace, where it is fixed and pinned.

## Facts to confirm, and the build order

To confirm or build in the code:

- One successful record-store session leaves both sides holding the union of their frozen views, whichever side dialed, the relay case included. No test isolates one session: `pod_delivery.rs` waits for convergence. This is step 0.
- The replica applies an insert, timestamp included, before any session freezes its view — confirmed for the data store in the grant-era step 0, and the same actor serves the pod stores.
- The dialing side sends `Abort` when its exchange fails, instead of closing cleanly (the `session-outcome` change).
- An event subscription for a pod store of one hosted identity, a record view with verdicts, and a pod-interval setting in `pdn-node-http` and in the facade's test build: none exists today (the `sync-observation` change, revised for pods).
- A node on `Connectivity::Direct` puts its LAN address in a pod invite, and a phone node on `for_product` joins through it.
- iroh tells per remote node whether the connection is direct or relayed.

To confirm in the tools, unchanged from the grant-era draft: `devicectl` launch environment and container copy, Local Network permission across relaunch and directory deletion, pf rules and counters, the macOS application firewall.

**Build order.** Each step is usable on its own, and the phone arrives last:

1. The single-session test for a pod store, in the workspace.
2. Network spike, by hand.
3. `pdn-node-http`: the pod event route, the record view route, the pod-interval setting.
4. Coordinator and the planted-defect suite, laptop only.
5. The facade's test build and the agent in `pdn-app`, then the steady and partition scenarios on the phone.
6. The remaining scenarios, then the network variants.

The coordinator is a Python script using only the standard library, run as `python3 -I`. It lives in `sync-test/` of the workspace; the phone's half — the test build, the agent and `phone.py` — in `pdn-app`, because that stages the application.
