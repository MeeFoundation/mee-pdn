# Sync reliability test: implementation plan

This plan builds two things over one observation surface: the load tests `load.md` describes (first), and the reliability test `design.md` describes (after them). It replaces the grant-based plan kept in `grants-era/plan.md` and fixes the interfaces the tasks share before any of them starts.

Every measurement runs on release builds: `pdn-node-http` built with `--release` and the `observation` feature, and the phone's test build in its release configuration. The coordinator refuses a debug build.

## Ground rules for every task

- No git state changes in any repository: a person commits. Read-only git is fine.
- The PDN workspace is `mee-pdn-linking-devices-v4/`. The spec tree is `mee-pdn/mia-docs/` (a sibling checkout on branch `linking-devices-v4`). `pdn-app/` and `pdn-sdk/` are repositories of their own inside the workspace.
- A behaviour change of `pdn-store`, `data-layer`, `pdn-node`, `pdn-node-http`, `pdn-mobile` or `pdn-app` is specified in an OpenSpec change in `mia-docs` first, and its code is written only after a person approves that change. A test, or a `test-util` passthrough that changes no behaviour, needs no change.
- Workspace rules apply as written in the root `CLAUDE.md`: tests through `just`, code comments brief and present tense, every test with a doc comment that opens with the case, paired denials, a flaky-test stress pass after a change that touches sync, Python stdlib-only and run as `python3 -I` with the isolated-mode guard.
- `grants-era/` is reference only: nothing there is run, copied or extended.

## What changed from the grant-era plan

- The reader holds the pod's stores as a member, not a grant-imported replica: an entry reaches it from any member device, so the qualifying session is any record-store session with a member that held the entry before its side started, of either origin.
- Arrival is not reading: the final listing carries each entry's verdict, and a present entry that does not count is a defect of its own (`uncounted`).
- The `partial` and `outsider` controls go. The control is `k`, a member removed during the run. The `h/` rewritten slots go: nothing in a pod is replaced; operations on a shared mergeable-document take their place.
- The unnudged listing goes: no pod read nudges a sync. The reconcile-interval setting becomes a pod-interval setting.
- Task 0a's `crates/pdn-node/tests/scoped_session.rs` and its two passthroughs (`read_unnudged`, `sync_scoped_once`) prove the grant path, not this test's. They stay as uncommitted work for a person to keep or drop; nothing below builds on them.
- Task 0b and the `session-outcome` change stand as they are: they are about the store's session reporting, which pods use unchanged.
- The `sync-observation` change in `mia-docs` was written for a grant-imported replica and is revised (task 2a).

## Shared interfaces

Fixed here so the host routes, the phone agent and the coordinator agree. A task that finds one of them unworkable stops and reports rather than changing it alone.

### Event record

Every node's events reach the coordinator as JSON Lines, one object per event. Times are integer microseconds since the Unix epoch on the clock of the node that produced them. Node ids, pod ids, hashes and keys are the lowercase text forms their types print; a key is its UTF-8 text.

| Field | Kinds | Meaning |
|---|---|---|
| `sub` | all | Subscription id, new on every (re)subscribe |
| `seq` | all | Counter per subscription, from 0, rising by 1; a gap means events were lost on that hop |
| `kind` | all | `insert_local`, `insert_remote`, `content_ready`, `pending_content_ready`, `sync_finished`, `lagged`, `closed` |
| `t` | all | When the host observed the event |
| `pod`, `store` | all | The pod, and `membership` or `records` |
| `key` | `insert_local`, `insert_remote` | The entry's key as text |
| `entry_ts` | `insert_local`, `insert_remote` | The entry's own timestamp, set at insert on the writing node |
| `hash`, `len` | `insert_local`, `insert_remote`, `content_ready` (`hash` only) | The payload's content hash and length |
| `from` | `insert_remote` | The node that sent it |
| `content_status` | `insert_remote` | `complete`, `incomplete` or `missing` |
| `peer`, `origin`, `session`, `started`, `finished`, `ok`, `error`, `error_kind`, `sent`, `received` | `sync_finished` | Counterpart node; `connect` or `accept`; a session id both sides derive from the connection's keying material; both session times; the result; the error and its stage (`connect`, `sync`, `close`, `remote_abort`); entries sent and received |

One subscription covers both stores of one pod for one hosted identity, numbered as one stream. Every `insert_remote` of a session precedes that session's `sync_finished` in `seq` order, and the subscription loses no event of a 1,000-entry session; any remaining loss shows as `lagged` or a `seq` gap. When the record store is forgotten (a removal or a leave) the stream emits `closed` for it and goes on for the membership store.

### Run directory

The coordinator writes everything under `tmp/sync-test/<run id>/` in the workspace, and the classifier reads only this directory. `classify.py`'s module docstring is the exact contract.

- `meta.json` — run id, git revisions, build profile, parameters, `t0_us`, `nodes` (name to `node`), `readers` (name to `pass_interval_s`), optional `removed` (`node` name and the removal's membership-store `key`), `phases`, optional `cut`, `cpu_cores`, planted defects.
- `journal.jsonl` — one line per attempted write: `writer`, `seq`, `kind` (`record` or `op`), `key`, `hash`, `t_write`, `sha256`, `result`.
- `events/<node>.jsonl` — every event record of that node, as received, plus the coordinator's receive time `rt`.
- `listing/<reader>.json` — the final record view as a list, one item per entry: `{key, author, hash, len, present, payload, verdict}`, `verdict` one of `counted`, `not_yet`, `for_nothing`, `outside_layout`. A key can hold several entries — one per author — so the list is never reduced to a map by key.
- `clock.jsonl` — `reader`, `offset_us`, `bound_us`, `t`.
- `health.jsonl` — `cpu_pct` or `app_state` lines.

## Step 0 — confirm the guarantee, and fix what the design depends on

### Task 0a: the single-session test for a pod store

In `crates/data-layer/tests/` (the pod stores and their test hooks live in `data-layer`). Three members on three nodes with the pod interval at 1 hour, every store out of reach of gossip for the measured part (`leave_swarm_for_test`). After the join catch-ups have finished: A places records; B pulls them from A; then exactly one session R–B (`sync_pod_with_for_test`, membership store then record store), dialed by R in one case and by B in the other. R then holds every record B held when its side started, and they count in R's record view. Paired denial in the same test: a record A placed after B's session started is absent on R. Broken on purpose: without the session call the test fails. Stress it.

Done when: the test passes under stress, the broken run fails, and the report records, with file and function names, whether a record-store session that fails mid-exchange leaves the membership store's earlier session counted.

### Task 0b: the dialer aborts a failed exchange

Unchanged from the grant-era plan: the `session-outcome` change in `mia-docs`, then the fix in `crates/pdn-store/src/net.rs` with a test that fails without it, after a person approves the change.

## Step 1 — network spike (a person, with the phone)

As in the grant-era plan, with one addition: the phone joins a pod on an invite minted by a `pdn-node-http` on `PDN_CONNECTIVITY=direct` bound to the LAN, and lists a record placed there. Written into `spike.md` as it is done.

## Step 2 — observation surface

### Task 2a: revise the OpenSpec change `sync-observation`

In `mia-docs/openspec/changes/sync-observation/`, rewritten for pods, following `mia-docs`' own `CLAUDE.md` and `.claude/rules/openspec.md`, with its sweep of the active changes (`mobile-host-surface`, `mobile-demo-app`, the `pods-*` follow-ups):

- `pdn-store` and `data-layer`: the ordering guarantee and the loss bound of the event record, `error_kind` and the sent and received counts on the session event, the session id, the `closed` event when a store is forgotten.
- `pdn-node`, behind `test-util`: a subscription to both stores of one pod for one hosted identity yielding the event record; the record view with verdicts (`pod_record_view_for_test` exists and returns `RecordView`).
- `pdn-node-http`: the cargo feature `observation = ["pdn-node/test-util"]`, off by default; `PDN_POD_RECONCILE_INTERVAL_SECS` (unset keeps the runtime default, `0` or a non-number stops the start); debug routes for the two runtime calls, the event route streaming JSON Lines over one long response and not holding an admission permit, with a bound on concurrent streams.
- `pdn-mobile`, behind its `observation` feature, which the release app does not enable: the laptop host's debug and observation routes served on the phone's loopback, beside a reset at a pod interval and a connectivity, the device samples and the device's clock.
- `pdn-app`, in `mobile-common`: the test build in its release configuration, the agent screen, the launch environment through a native module, the test surface started on the phone's loopback and reached over the cable through usbmux, and a device sample once a second: the CPU of the process and of the node's threads, resident memory, the thermal state, Low Power Mode, the battery and the application state.
- The release requirement: the `observation` feature compiles in a release build, and each host reports its build profile on its status, so a coordinator can refuse a debug build.
- Operating conditions walked: several identities on one node (the peer is a node, not a member — a stated limit, and the in-process path between two co-located members reports no network session), a restart (subscriptions do not survive it), a removal and a leave (the record store's stream closes), a disk that fills (the agent's journal), clocks that disagree.
- Access control: the routes exist only with the feature and `PDN_DEBUG`; the facade calls only in the test build.

Done when the change validates and a person approves it.

### Task 2b: implement `sync-observation`

After approval. Tests:

- a unit test of numbering, `sub`, `lagged` and `closed`;
- one session delivering 1,000 records yields every `insert_remote` before its `sync_finished`, with no `lagged`;
- a container test: two hosts with the feature in one pod; a record placed on one shows on the other's event route as an `insert_remote` and a paired `sync_finished` on each side with matching counts, and in its record listing as counted. Paired denial: a removed third host that has folded the removal receives no record placed after it, and its record-store stream ends in `closed`;
- the host without the feature, and with it but without `PDN_DEBUG`, answers 404 on the routes.

Stress the new scenario tests. Done when `just check`, `just test`, the container tests and the stress pass are green.

## Step 3L — the load tests, laptop first

All in `sync-test/`, Python stdlib, `python3 -I`: `load.py` (the runner, under `caffeinate`: preflight, forms a pod of N nodes, fills it level by level, has `r` leave and join again for every catch-up, runs idle, steady and catch-up per cell under the deadlines of `load.md` "Running the matrix unattended", samples CPU with `ps`, collects events into the run directory, marks the cell, appends `load.csv`, resumes a run by its id, cleans up after each cell and each N, owns the pf anchor for L-net) and `analyze.py` (pure: reads `load.csv`, fits each metric's slopes in N and V per test, computes the ratio between the tests and states the verdict per metric, as `load.md` "Is the laptop honest?" describes).

`load.csv` has one row per cell and repeat: `test` (L, P), `variant` (plain, net, slow; phone), `N`, `V` (the level), `V_actual` (the records the pod held when the cell started), `S`, `repeat`, the marks `contended`, `lagged` (a count), `incomplete`, `join_hung`, `not_run`, `inactive`, `low_power`, `slept`, `not_quiet`, `thermal`, `pod_pass`, and `evidence` (a path in the run directory for a `join_hung` cell), and the metrics — `steady_p50_s`, `steady_p90_s`, `steady_p99_s`, `catchup_s`, `catchup_records_per_s`, `r_cpu_s_per_record`, `sessions_per_record`, `idle_r_cpu_pct`, `idle_long_r_cpu_pct` (first repeat only), `r_rss_mb`, and in test P `observation_cpu_s` (the process total less the node's threads). `sessions_per_record` is the distinct record-store `session` ids with `received` above 0 across all nodes during steady, divided by the records placed.

### Task 3L-a: the analysis, on synthetic data

`analyze.py --synthetic` builds `load.csv` tables whose verdicts are known — a constant ratio with equal slopes, a V slope that differs, a phone table too noisy to decide, a ratio that wanders, the empty pod as a baseline, `V_actual` against the level, contended cells, lagged cells in a time and in an events metric, incomplete cells, a zero value, a laptop-only table — and asserts each verdict; each check fails with its rule broken. Buildable now: it reads only the table.

### Task 3L-b: test L, plain

After 2b. A pilot at N = 2 and 4 first: it replaces `load.md`'s running-time estimate, a person decides from it between 3 repeats and 2, and it passes `load.md`'s pilot gates — zero `lagged` at V = 100,000, the laptop's observation cost measured — before the matrix is scheduled. Then the plain variant over the matrix. Done when the matrix runs unattended to its end, an interrupted run resumes by its id, and `analyze.py` reports on test L alone (slopes, no ratio yet).

### Task 3L-c: test P

After 4b. The same matrix with the phone as `r` and as the joiner, `load.py --variant phone` (`load.md`, Running test P); the run records the clock offset and its bound before each incarnation of `r`. Done when `analyze.py` reports the verdict per metric against plain L.

### Task 3L-d: L-net and L-slow

After 3L-c, because L-net's pipe is sized from what 3L-c measured. The sudoers grant for `dnctl` and `pfctl` is a person's step, done once; without it L-net runs attended. Done when `analyze.py` reports the verdict per metric against each variant, and the report names the variant that is honest where plain L is not.

## Step 3 — coordinator and the planted-defect suite, laptop only

All in `sync-test/`, Python stdlib, `python3 -I`, every file with the isolated-mode guard: `classify.py` (pure: reads a run directory, writes `report.txt`, appends to `runs.csv`, exits 0 PASS, 1 FAIL, 2 INCONCLUSIVE), `run.py` (the coordinator) and `selfcheck.py` (the synthetic checks and the planted-defect suite).

The laptop reader `r` stands in for the phone. The laptop cut is `SIGSTOP` and `SIGCONT` of `r`'s process; stopping a writer for the author-offline scenario is the same signal.

### Task 3a: the classifier, on synthetic data

Done: `python3 -I sync-test/selfcheck.py --synthetic` builds one small run directory per state and finding of the design's tables, asserts the classifier reports exactly that, and asserts each case fails with its rule broken.

### Task 3b: the coordinator and the planted defects on real nodes

After 2b and 0b. Default parameters small enough for a laptop run of a few minutes (3 writers, 200 records, 10 per second, pod interval 30 seconds); the main run's parameters by flag. Every row of the design's planted-defect table is a flag of `run.py` that `selfcheck.py` runs and checks against its expected report. Done when every row gives exactly its expected report and a clean run repeated 10 times gives PASS every time.

## Step 4 — the phone

- **4a.** The facade's test build in `pdn-mobile`, and `pdn-sdk`'s `test-build`. Built.
- **4b.** The agent in `pdn-app`: the agent screen, the launch environment through a native module, the test surface on the phone's loopback, the device sample, app-state reporting; the reset, the clock, the acts and the record view are the surface's routes, and `phone.py` builds, installs and drives it (`load.md`, Running test P). Built.
- **4c.** `run.py --phone`: the `devicectl` launch and the tunnel of `phone.py`, `pfctl` rules and probe, then the steady and partition scenarios end to end.

Done when a steady and a partition run on the phone produce a verdict, and the planted defects that do not need a laptop reader give their expected reports with the phone as reader.

## Step 5 — remaining scenarios and network variants

Author offline, lossy, restart, join under load, fan-out, same identity, leave; then a router shaper, LTE with relays, a second phone. Planned after step 4 on what it teaches.

## Order and parallelism

| Now, in parallel | After a person approves 2a | After 2b | After step 1 and 2b | After 4b |
|---|---|---|---|---|
| 2a, 3L-a; 3a (done) | 2b | 3L-b; then 0a, 0b, 3b for the reliability test | 4a, 4b | 3L-c, then 3L-d, then 4c |
