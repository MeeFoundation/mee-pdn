# Sync reliability test: implementation plan

This plan builds the test that `design.html` in this directory describes. It follows the design's build order, splits every step into tasks one agent can finish and prove, and fixes the interfaces the tasks share before any of them starts, so that tasks built in parallel meet without renegotiation. A review against the code changed the design in five places, listed at the end; the plan already follows them.

## Ground rules for every task

- No git state changes in any repository: a person commits. Read-only git is fine.
- The PDN workspace is `mee-pdn-linking-devices-v4/`. The spec tree is `mee-pdn/mia-docs/` (a sibling checkout on branch `linking-devices-v4`; this workspace has no `mia-docs/` of its own). `pdn-app/` is its own git repository inside the workspace.
- A behaviour change of `pdn-store`, `data-layer`, `pdn-node`, `pdn-node-http`, `pdn-mobile` or `pdn-app` is specified in an OpenSpec change in `mia-docs` first, and its code is written only after a person approves that change. A test, or a `test-util` passthrough that changes no behaviour, needs no change.
- `crates/pdn-mobile/src/node.rs` and `crates/pdn-mobile/tests/surface.rs` carry a person's uncommitted edits (an `identity` parameter on `write_entry`, `read_entry`, `list_entries`). No task edits these files until that work is committed; a task that needs them stops and says so.
- Workspace rules apply as written in the root `CLAUDE.md`: tests through `just`, code comments brief and present tense, every test with a doc comment that opens with the case, paired denials, flaky-test stress pass after a change that touches sync, Python stdlib-only and run as `python3 -I` with the isolated-mode guard.
- The old `pdn-app/demo` and `pdn-app/demo-loadtest` are not used, copied or referenced.

## Shared interfaces

These are fixed here so the host routes, the phone agent and the coordinator agree. A task that finds one of them unworkable stops and reports rather than changing it alone.

### Event record

Every node's events reach the coordinator as JSON Lines, one object per event. Times are integer microseconds since the Unix epoch on the clock of the node that produced them. Node ids, namespace ids and hashes are the lowercase text forms their types print.

| Field | Kinds | Meaning |
|---|---|---|
| `sub` | all | Subscription id, new on every (re)subscribe |
| `seq` | all | Counter per subscription, from 0, rising by 1; a gap means events were lost on that hop |
| `kind` | all | `insert_local`, `insert_remote`, `content_ready`, `pending_content_ready`, `sync_finished`, `lagged` |
| `t` | all | When the host observed the event; informational, never used to order an arrival against a session |
| `namespace`, `issuer` | all | The subscription's namespace and the identity that issues it |
| `path` | `insert_local`, `insert_remote` | The entry's path as text |
| `entry_ts` | `insert_local`, `insert_remote` | The entry's own timestamp, set at insert on the writing node |
| `hash`, `len` | `insert_local`, `insert_remote`, `content_ready` (`hash` only) | The payload's content hash and length |
| `from` | `insert_remote` | The node that sent it |
| `content_status` | `insert_remote` | `complete`, `incomplete` or `missing` |
| `peer`, `origin`, `session`, `started`, `finished`, `ok`, `error`, `error_kind`, `sent`, `received` | `sync_finished` | Counterpart node; `connect` or `accept`; a session id both sides derive from the connection's keying material (absent when no connection was made); `connect` (this side dialed) or `accept`; both session times; the result; the error text and its stage (`connect`, `sync`, `close`, `remote_abort`, from the `ConnectError`/`AcceptError` variant); entries sent and received (`SyncDetails`) |

The stream is ordered: every `insert_remote` of a session precedes that session's `sync_finished` in `seq` order. `Engine::subscribe` in `crates/pdn-store/src/engine.rs` merges two channels without that guarantee today, so providing it is part of task 2a's change. The subscription loses no event of a 1,000-entry session: the replica channel is lossy at 256 (`SUBSCRIBE_CHANNEL_CAP`, `subscribers.rs`), so 2a sizes or replaces that for observation. Any remaining loss shows as `lagged` or a `seq` gap and makes pairs undetermined.

### Classification against this record

The design's rule, restated on stream order instead of times:

- **Pairing the two sides.** By `session`: a reader-dialed session and a writer-accepted one with the same id are the two sides of one session. Ordered pairing does not hold: a writer refusing before the pair registers, or a timeout mid-exchange, leaves one side with no counterpart. A side with no counterpart is a one-sided session: never qualifying, reported under session health. Each pair is checked: the reader's `received` equals the writer's `sent`; a mismatch makes the pair undetermined from that session on.
- **Qualifying session.** Both sides ok, and the writer's `started` later than the entry's `entry_ts` (both on the writer's clock).
- **Violated.** The entry has no `insert_remote` on the reader before that session's `sync_finished` in the reader's `seq` order.
- **Split sessions.** A split with `error_kind=close` on the failing side, after an exchange whose counts match, is reported as a teardown split: counted, not a defect. The `session-outcome` change keeps a teardown error failing the session on purpose, because a clean end of stream can also come from a dropped session. Every other split is a defect.

### Run directory

The coordinator writes everything under `tmp/sync-test/<run id>/` in the workspace, and the classifier reads only this directory:

- `meta.json` — run id, git revisions, build profile, parameters, node names to node ids and to issuer ids, reader reconcile intervals, phase boundaries, the `partial` control's expected set, planted defects.
- `journal.jsonl` — one line per attempted write: `writer`, `seq`, `path`, `version` for `h/` slots, `sha256` of the payload fields, `result` (`ok` or `refused` with a reason), and `t_write` and `hash` copied from the writer's `insert_local` for that path.
- `events/<node>.jsonl` — every event record of that node, as received, plus the coordinator's receive time `rt`.
- `listing/<reader>.json` — the final listing: per issuer, every path with its content hash, length, whether the payload is present, and the payload itself.
- `clock.jsonl` — clock samples: `reader`, `offset_us`, `bound_us` (half the round trip), `t` on the coordinator's clock.
- `health.jsonl` — lines with `cpu_pct` (node processes) or `app_state`.

`classify.py`'s module docstring is the exact contract for these files: `meta.json` carries `t0_us`, `nodes` (name to `node` and `issuer`), `readers` (name to `interval_s` and `covers`, each writer mapped to `"all"` or an explicit list of paths), optional `cut` and `cpu_cores`; the payload's `sha256` is SHA-256 over its other fields as compact JSON with sorted keys; the listing is `{issuer: {path: {hash, len, present, payload}}}`. A laptop reader with no `clock.jsonl` sample gets offset 0 and no clock check, so the planted clock shift writes one.

## Step 0 — confirm the guarantee, and fix what the design depends on

### Task 0a: the single-session test

Done: `crates/pdn-node/tests/scoped_session.rs`, 300 of 300 under stress, fails with the session call removed. Findings: the timestamp and the view freeze are messages on one replica actor, so the atomicity holds; a writer-dialed session to a grantee always fails with `NotFound`, so each writer's stream carries one failed `connect` per pass per grantee; a 1,016-claim grant is 92,836 bytes and arrives in about 150 ms.

In `crates/pdn-node/tests/` (the grant binder lives in `pdn-node`, not in `data-layer`). Two `test-util` passthroughs on the runtime: `read_unnudged` and one scoped session of a grantee against one contact. The test: both nodes on a reconcile interval of 1 hour; the writer publishes a grant, the grantee imports it through `sweep_pair_now`; after the import's own session has finished, the writer writes covered and uncovered entries; the grantee runs exactly one scoped session; `read_unnudged` then finds every covered entry and no uncovered one. Paired denial in the same test: the uncovered entry is absent. If the one-session call bypasses the dial-side session access that a product session has (the existing `sync_as_for_test` passes egress and ingest `None`), the passthrough uses the product's scoped path instead, or the test doc names what is bypassed and why that leaves the claim intact. Broken on purpose: without the session call, the test fails. Stress it (`just stress`, a few hundred iterations on this test).

The same agent records, with file and function names, and without code changes: that the entry timestamp is taken inside the replica's insert before any session can freeze a view; what a writer-dialed session to a grantee does (fails, or succeeds carrying nothing); whether a grant of 1,016 exact-path claims fits the entry payload limit.

Done when: the test passes under stress, the broken run fails, and the three facts are in the agent's report.

### Task 0b: the dialer aborts a failed exchange

`connect_and_sync_inner` in `crates/pdn-store/src/net.rs` finishes its send stream cleanly after `run_alice` fails, and `run_rounds` in `net/codec.rs` reads the clean end as a finished session, so the accepting side reports ok for a session the dialing side failed. The fix sends `Abort` from inside `run_alice`, where the framed writer is; the acceptor already fails on a received `Abort`. The same task decides, with its argument under `defect-reachability.md`, whether a teardown error after a completed exchange (`closed?` in `connect_and_sync_inner`, `res.and_then(closed)` in `handle_session_inner`) should still fail the session.

First as an OpenSpec change `session-outcome` in `mia-docs` (no store spec covers session outcomes today): both sides of a session report the same outcome, with scenarios for a dialer failure midway and a teardown error. Code after approval: a test in `crates/pdn-store` that fails without the fix, the `Message::Abort` doc updated, the divergence recorded in `crates/pdn-store/CLAUDE.md` and `UPSTREAM.md` as `.claude/rules/pdn-store.md` requires, then `just check`, `just test`, `just test-store` and a stress pass of the store's session tests. The code starts after 0a's stress run, because 0a exercises the path 0b edits.

## Step 1 — network spike (a person, with the phone)

By hand, about half a day, with the steps written into `pdn-app/sync-test/spike.md` as they are done: a `pdn-node-http` on `PDN_CONNECTIVITY=direct` bound to the LAN, an invite consumed by the phone app, the entry read on the phone; `pfctl` rules for the phone's address on the interface `route get` names, a UDP probe and the rule counters; that TCP to the phone passes while UDP is dropped; the macOS application firewall prompt or rule for the binary; `xcrun devicectl device process launch` with environment variables, and `xcrun devicectl device copy from` on the app's data container; that Local Network permission survives relaunch and deleting the node directory. Needs `sudo` for `pfctl`. No agent does this step; a failed fact changes the design before step 5.

## Step 2 — observation surface on the runtime and the laptop host

### Task 2a: OpenSpec change `sync-observation`

In `mia-docs/openspec/changes/sync-observation/`: proposal, design, tasks and delta specs, following `mia-docs`' own `CLAUDE.md`, `.claude/rules/openspec.md` (including its sweep of other active changes: `mobile-host-surface` and `mobile-demo-app` are active) and an existing change as the model (`reconcile-trigger`). Content:

- `pdn-store` and `data-layer`: what the subscription needs from them — the ordering guarantee and the loss bound of the event record above, `error_kind` and the sent and received counts on the session event.
- `pdn-node`, behind `test-util`: a subscription to one hosted identity's replica of one issuer yielding the event record; an unnudged listing returning, per path, content hash, length, whether the payload is present, and the payload.
- `pdn-node-http`: a cargo feature `observation = ["pdn-node/test-util"]`, off by default; the stand image builds without it and a separate build argument turns it on; `scripts/test-features.sh` and `just check` cover both builds. `PDN_RECONCILE_INTERVAL_SECS`: unset keeps the runtime default, `0` or a non-number stops the start, as the host's other settings do. Debug routes for the two runtime calls, the event route streaming JSON Lines over one long response. Each route delegates to one runtime call, as the host spec requires. An open stream does not hold an admission permit (`admit_request` releases it with the headers); the change says so and bounds concurrent streams.
- `pdn-mobile`, behind a test-build feature the release app does not enable: the same two calls in the facade.
- `pdn-app`, in `mobile-common`: the test build, the agent screen, the control channel over HTTP to the coordinator, the forwarded event stream, the local journal, the launch environment through a native module.
- Operating conditions walked: several identities on one node (the event's peer is a node, not an identity: stated as a limit), a restart (subscriptions do not survive it; the agent re-subscribes with a new `sub`), a disk that fills (the agent's journal), clocks that disagree (the classification never compares two devices' clocks).
- Access control: the routes exist only with the feature and `PDN_DEBUG`; the facade calls only in the test build.

Done when: the change validates with the repository's OpenSpec tooling and a person approves it.

### Task 2b: implement `sync-observation` for the store, data layer, runtime and host

After approval. Tests:

- a unit test of numbering, `sub`, and `lagged`;
- one session delivering 1,000 entries yields every `insert_remote` before its `sync_finished`, with no `lagged`;
- a container test (the host's convention): two hosts with the `observation` feature sync one covered entry; the reader's event route shows `insert_remote`, `content_ready` and a `sync_finished` on each side with matching counts; the writer's shows `insert_local` with the same `entry_ts`. Paired denial: an uncovered entry written in the same run produces no `insert_remote` on the reader;
- the unnudged listing starts no session: both hosts on `PDN_RECONCILE_INTERVAL_SECS=3600`, no `sync_finished` with `origin=connect` on the reader within 10 seconds of an unnudged listing, and a nudged list in the same setup as the positive control that does produce one;
- the host without the feature, and with the feature but without `PDN_DEBUG`, answers 404 on the routes.

Stress the new scenario tests. Done when `just check`, `just test`, the container tests and the stress pass are green.

## Step 3 — coordinator and the planted-defect suite, laptop only

All in `pdn-app/sync-test/`, Python stdlib, `python3 -I`, every file with the isolated-mode guard. Three files and no more until one grows past reading:

- `classify.py` — pure: reads a run directory, writes `report.txt`, appends to `runs.csv`, exits 0 on PASS, 1 on FAIL, 2 on INCONCLUSIVE. Implements every state, finding, metric and validity check of the design, with the classification section above; where the design is silent it says so in the report rather than guessing.
- `run.py` — the coordinator: starts laptop nodes from a `pdn-node-http` built with `observation` (path given by a flag), connects and grants through the debug routes on the product path, subscribes to events, writes on schedule, cuts and restores, waits for quiescence, collects listings, calls the classifier.
- `selfcheck.py` — the synthetic checks and the planted-defect suite.

The laptop reader is a `pdn-node-http` standing in for the phone, on the phone's 5-second reconcile interval. The laptop cut is `SIGSTOP` and `SIGCONT` of a process: it works on macOS and Linux without root. Stopping the reader loses every session's reader side; stopping one writer for 30 seconds makes the reader log dialed sessions the writer never saw, which exercises the pairing rule. A reader still stopped at the end of the run is not listed; its pairs stay stalled and the run is INCONCLUSIVE.

### Task 3a: the classifier, on synthetic data

Done: 22 synthetic cases pass, and each fails with its rule broken. Buildable, before step 2, because it reads only the run directory. `selfcheck.py --synthetic` builds small run directories — one per state and finding of the design's tables, each with its expected report — and asserts the classifier reports exactly that. Includes the worked example of entry 417 with its numbers, both clock-check directions, the stalled deadline at 5 × the interval, a `seq` gap and a new `sub` making pairs undetermined, reader-only failed sessions that must not shift the pairing, a sent and received mismatch, a teardown split, only the last `h/` version expected, refused writes excluded, and the `partial` control's set.

Done when: `python3 -I pdn-app/sync-test/selfcheck.py --synthetic` passes, and each case, with the classifier's rule for it deliberately broken, fails.

### Task 3b: the coordinator and the planted defects on real nodes

After 2b and 0b. Default parameters small enough for a laptop run of a few minutes (3 writers, 200 entries, 10 per second); the main run's parameters by flag. Planted defects, each a flag of `run.py` that `selfcheck.py` runs and checks against its expected report. Three of them rewrite data inside the harness and so test the classifier, not the platform; they are marked so in the report.

| Planted defect | How the harness plants it | Tests |
|---|---|---|
| journaled but never written | `run.py` journals `s/000000500` of `w2` with a `t_write` from the writer's host clock and skips the write | classifier |
| wrong sha | `w3` writes one payload with a wrong `sha256` field | platform path |
| extra | `w1` grants `partial` slot 7 as well | platform path |
| split session | the coordinator rewrites the reader's side of one session to an error on ingest | classifier |
| partition never lifted | the reader stays stopped to the end | platform path |
| clock shifted | the classifier is given a reader offset of +2 s, then −2 s | classifier |
| slow reader | the reader's interval is 30 s | platform path |
| event hop drops | the coordinator drops a range of `seq` from one node's events on ingest | classifier |
| writer stopped | one writer stopped for 30 s | platform path, pairing |

Done when: every row gives exactly its expected report, and a clean run repeated 10 times gives PASS every time.

## Step 4 — OpenSpec for the phone side

Covered by task 2a's change, `pdn-mobile` and `pdn-app` parts. Implemented after step 1 confirms the tools, and after the person's `pdn-mobile` edits are committed.

## Step 5 — the phone agent, then steady and partition on the phone

- **5a.** The facade's test-build calls in `pdn-mobile`, and a `pdn-sdk` build with them.
- **5b.** The agent in `pdn-app`: the agent screen, the launch environment through a native module, the HTTP control channel, the forwarded and locally journaled events, the reset that deletes the node directory, the clock ping, app-state reporting. Commands from the coordinator: reset, consume invite, list unnudged, ping.
- **5c.** `run.py --phone`: `devicectl` launch and journal pull, `pfctl` rules and probe (with `sudo`), then the steady and partition scenarios end to end.

Done when: a steady and a partition run on the phone produce a verdict, and the planted defects that do not need a laptop reader give their expected reports with the phone as reader.

## Step 6 — remaining scenarios and network variants

Lossy, restart, fan-out, same identity, join under load, withdraw; then a router shaper, LTE with relays (with a per-connection direct or relayed report from the endpoint, `Endpoint::remote_info` in iroh), a second phone. Planned after step 5 on what it teaches.

## Order and parallelism

| Now, in parallel | After a person approves 2a and `session-outcome` | After 2b and 0b | After step 1 and the facade edits are committed |
|---|---|---|---|
| 0a, the `session-outcome` change, 2a, 3a | 2b, then 0b's code after 0a's stress run | 3b | 5a, 5b, 5c |

2a, the `session-outcome` change and 3a touch no Rust, so they run beside 0a without disturbing its builds.

## Changes to the design

The review against the code changed the design here; `design.html` and `design.ru.html` are updated to match once 2a settles them:

- Arrival against a session is decided by order in the reader's event stream, not by comparing an observation time with `finished`.
- The two sides of a session are paired after dropping dialer failures the writer never saw, and checked by sent and received counts.
- The `outsider` control goes: the host surface hands over no ticket, and `partial` already holds w1's ticket without a claim to the odd slots, the tightest denial. The outsider case stays covered by `crates/data-layer/tests/read_restriction.rs`.
- The download-retries metric goes: no event reports a failed download.
- `/debug/paths` moves to the relay variant: laptop nodes on `direct` have no relay, so every connection to them is direct.
