# Pod load tests: laptop only, and with the phone

Design draft, 2026-10-07, revised after review the same day. Two load tests over one matrix, and the comparison between them. The reliability test in `design.md` answers whether an entry is lost; these answer what a pod costs and how fast it is, and whether a laptop can stand in for the phone when the question is asked again.

## The questions

1. How do CPU and sync speed depend on the number of member nodes in a pod and on the volume the pod holds?
2. Does a pod whose nodes all run on the laptop measure what a pod with the phone in it would — and for which of the numbers?

The second question decides how the project load-tests from here on: a laptop run needs no person, a phone run needs a phone on a cable. If the laptop is honest for a metric, that metric is load-tested on the laptop alone.

## The two tests

| | Test L: laptop only | Test P: with the phone |
|---|---|---|
| Members | N `pdn-node-http` processes on the laptop | N − 1 on the laptop, and the phone |
| Measured node `r` | one laptop node | the phone |
| Path to `r` | loopback | Wi-Fi, direct UDP |
| Build | release, the `observation` feature | laptop: the same; phone: the application's test build in its release configuration |

Every number comes from a release build. The coordinator reads the build profile from each node's status and from the phone's reset, refuses to measure against a debug build, and records the profiles in the run's `meta.json`: a debug build of the store is several times slower, and a matrix measured on one says nothing about the product.

`r` is the one node whose numbers are compared between the tests. It writes nothing; the other members write. Every node hosts one identity, so a node is a member.

## The matrix

| Factor | Levels |
|---|---|
| N, member nodes, `r` included | 2, 4, 8, 16 |
| V, records the pod holds when a cell starts | 0, 1,000, 10,000, 100,000 |
| S, payload size | 256 B; 64 KiB at V up to 10,000 |

Per test, 16 cells at 256 B and 12 at 64 KiB, each repeated 3 times in a random order of N. 64 KiB × 100,000 records is 6.5 GB per member, 100 GB on the laptop at N = 16, and is left out by design.

V grows inside one pod: for each N and each S the coordinator forms one pod, runs the cells at V = 0, fills the pod up to 1,000 records, runs the cells, and so on. A pod is never reused across N or across S, so the pod's own history — joins, warm-up — is the same at every level of V, and a 64 KiB cell never holds 256 B records. The 3 repeats of a level run one after another on the same pod, so each repeat starts with the records the earlier ones placed during steady, about 600 more each; every row records `V_actual`, the records the pod held when the cell started, and the fit is over that count, not over the level's name.

**Payloads are unique.** Every record's payload carries the run, its writer and its sequence number, padded to S, so no two records share a content hash. The store reports `content_ready` once per content hash and never for a payload the node already holds, so a filler repeated across records would leave every record but the first without the event that latency and the fill's end are read from. An `insert_remote` whose content status is `complete` counts as ready at its own time.

**Filling** is not measured. Every writer places records one call after another, as fast as its node answers, all writers at once, each record of the cell's size S. The fill ends when `r`'s event stream holds a `content_ready` for every record placed, confirmed by one listing of `r` at the end; nothing polls the listing while the fill runs, since a listing at V = 100,000 is itself load on `r`. The fill rate is recorded so a slow fill is visible, and nothing is computed from it.

## When the platform cannot finish a cell

A cell the platform cannot finish is a result, not a gap: the run measures every cell of the matrix, and where the product hangs it says so, at which N, V and S, and with what evidence. The case this was built for was the store's join deadlock — a join whose first session inserted more than 1,024 entries hung for good, the live actor waiting on the store actor in `start_sync` while the store actor waited for room in the live actor's 1,024-event channel (`tmp/pdn-store-join-deadlock.md` in the workspace); it is fixed on main (`DrainingSync` in `crates/pdn-store`), and the catch-up workload at V = 10,000 and 100,000, which ran straight into it, is where a hang of that shape would show again.

- **Detected, not waited out.** The join call has its catch-up deadline (below). A join that has not answered by then, while its node's event stream has gone silent for 30 seconds, marks the cell `join_hung`; one that is still receiving events is `incomplete` instead.
- **Evidence kept.** For a hung join the coordinator saves the last 200 events of both nodes, both nodes' logs since the join started, and a stack sample of the joining process (`sample <pid> 5` on macOS) into the run directory, so the report points at what stalled rather than at a timeout.
- **The rest of that pod.** A hung join leaves the pod's `r` broken: the coordinator kills it, and every later cell of that N and S is recorded `not_run: join_hung` rather than measured on a pod in an unknown state. The next N starts from a fresh pod as usual.
- **Reported first.** The report opens with a table of the cells the platform did not finish — test, variant, N, V, S, repeat, mark, evidence path — before any metric, and the fits leave those cells out. The table is the answer to "at which parameters does it not work", in both tests, so it also shows whether the phone hangs where the laptop does.

## Workloads in a cell

Each cell runs three workloads in order, with the pod interval at its default (300 seconds) and recorded: the product's own cadence is what is measured.

**Idle**, 60 seconds; in the first repeat of a cell, 330 seconds instead, so that one window holds a whole pod interval. Nobody writes. CPU of every node: the gossip keep-alive and the armer's sweep in every repeat, the periodic pod pass in the first. The report gives both and says which is which.

**Steady**, 60 seconds, then up to 60 seconds for `r` to hold every record placed. The writers place records of size S at an aggregate rate of 10 a second, spread evenly. Per record: the latency from placement to readable on `r`. Per node: CPU. Per record placed: the sessions that carried it across the pod — the gossip cascade's fan-out.

**Catch-up.** `r` itself is the joiner, in both tests alike: it leaves the pod, its node's directory is deleted — the phone's storage is reset — and it joins again as a fresh identity on an invite from a writer. The pod therefore holds N − 1 members during the join and N after it, in every cell and in both tests, and no departed identity is left behind as a member: the pod derives its contacts from its current members only (`pod_contacts` in `crates/data-layer/src/node.rs`), so a member whose node is gone would otherwise be dialed by every member for the rest of the run. Each cell adds one joined and one left event to the membership store, the same number in both tests. The join goes through the observed join of the `sync-observation` change: it imports both stores, opens the subscription, and only then starts sync, so the first session's inserts are in the stream like every later one. The clock runs from the join call to the `content_ready` of the last record the pod held. The stream may lose events: the store's change subscription drops them rather than wait for its reader, and reports a `lagged` record. After a loss the count of `content_ready` stays short for good, so the joiner's own listing decides: the catch-up ends with the first listing that holds every record with its payload, and its time is the start of that listing. The listing is itself load on the joiner, so it is read once a second or, when the previous listing took longer than a quarter of a second, four listing durations apart; the end is therefore late by up to one such interval, and the catch-up record keeps the number of listings and the time they took, `listings` and `listings_s`, beside the last `content_ready` seen, `last_ready_s`, and the number of losses, `lagged`. Per cell: the catch-up time, records and bytes per second, the joiner's CPU, and the CPU of the members that served it.

## What is measured, and how

**Events.** Every node's pod events — `insert_remote`, `content_ready`, `sync_finished` per store — reach the coordinator from the node's event route — on the phone the same route of the test build's surface, reached over the cable (`Running test P`). The runtime takes each event off the store's subscription and stamps it in a task of its own, whatever pace the consumer reads at, so a coordinator that reads late still learns when the event happened. Nothing polls: a pod read starts no session, but it rebuilds the whole record view, so a poll at V = 100,000 would itself be load on `r`, and `r`'s load is what is compared.

**Latency** of a record on `r`: its `content_ready` on `r`, corrected by `r`'s clock offset (phone) or not (laptop), minus the record's own timestamp on its writer's clock. p50, p90, p99 per cell.

**Catch-up time**: from the join call to the `content_ready` of the last record the pod held, on the joiner's clock. Throughput is the records and bytes held divided by that time.

**Sessions per record**: the distinct record-store `session` ids with `received` above 0, across all nodes during steady, divided by the records placed. Both sides report a session, and it counts once; a session that carried nothing does not count. The gossip cascade is one pull per hop, so this grows with N and is where the N-dependence of CPU is expected to come from.

**CPU seconds per record** divides `r`'s CPU during steady by the records `r`'s final listing holds from steady, not by the events counted, so a lost event does not inflate it. The listing is taken without payloads, and steady's records are told by the ids their placements answered: a phone holding every payload of a large pod at once would be killed for memory.

**CPU on the laptop**: `ps -o cputime=` per node process every second; reported as CPU seconds per phase and as the average and peak percent of one core. Host total CPU from the same samples.

**CPU on the phone**: every second the agent samples, through its native module, the CPU time of the node's threads — the facade's runtime threads, named `pdn-node…`, the store's actor threads, named `sync-actor`, and the blob store's own runtime threads, named `iroh-blob-store-…` (`thread_info` per thread) — and, beside it, the whole process (`getrusage(RUSAGE_SELF)`). A thread a library starts under another name counts toward the remainder, so the observation's cost below is an upper bound. The node's share is what is compared with the laptop node. The test surface's server runs on the facade's runtime, so the work of serving the events is in the node's share, as the laptop host's server is in a laptop node's process; the remainder — the application's screen, the sampler, and any thread no one named — is reported over steady as `observation_cpu_s`, not subtracted from a guess. Resident memory (`phys_footprint`) comes with the same sample. The thermal state, Low Power Mode, the battery's state and level, and the application state are recorded with every sample.

**Work and time are reported apart.** CPU seconds per record received is work: it does not change when other processes compete for the CPU. Latency and catch-up time are time: they do. The comparison below leans on the difference.

**Memory**: resident size per node at the end of each workload, taken once the node's observation buffer has drained: the observed join's buffer holds the catch-up's records until the consumer reads them, tens of MB at V = 100,000, and that is the harness's memory, not the node's.

## A cell's validity

A cell is measured whole or marked; a marked cell is kept in `load.csv` and the analysis decides what it may be used for.

| Mark | When | The analysis |
|---|---|---|
| `contended` | host CPU above 70% of the laptop's cores during the cell | left out of time metrics |
| `lagged` (a count) | any `lagged` record or `seq` gap in a stream of the cell | left out of time metrics and of metrics counted from events (`sessions_per_record`) |
| `incomplete` | `r` (or the joiner) never held every record within its deadline | left out of every fit |
| `inactive` | the application left the active state during the cell (test P) | left out of every fit |
| `low_power` | Low Power Mode was on during the cell (test P) | left out of every fit |
| `slept` | the laptop's wall clock jumped against its monotonic clock by more than 5 s during the cell: it slept | left out of every fit |
| `join_hung` | the join did not answer by its deadline and the joiner's stream went silent (above) | left out of every fit; listed first in the report with its evidence |
| `not_run` | an earlier cell of the same pod hung (`not_run: join_hung`) | left out of every fit; listed with the hung cell |
| `not_quiet`, `thermal` | the wait before the cell ran out (below) | reported beside the verdict |
| `pod_pass` | a member's periodic pod pass falls inside steady or catch-up | reported beside the verdict |

A periodic pass runs every pod interval from its node's start, so the coordinator predicts each node's pass times from the start it recorded; the pilot checks the prediction against the sessions that appear while idle. A cell whose metric is not positive — an idle CPU that read 0 — enters no fit and is named in the report with the metric that read 0.

## Is the laptop honest?

For every metric m and every cell, the ratio ρ = m(test P) / m(test L) of the two tests' medians. The laptop is honest for m when two things hold:

- **The shape agrees.** m is fitted as log m = a + b·log N + c·log V over the cells whose pod held records, in each test. The empty pod is not a point of that fit: log(0 + 1) would sit far below the next level, and the V slope would describe the step from an empty pod rather than growth; the ratio at V = 0 is reported on its own as the empty-pod baseline. For each slope, the 95% interval of the difference between the tests must lie wholly within ±0.2. An interval wholly outside ±0.2 means the shapes differ; one that crosses the bound means this run cannot tell.
- **The ratio is a constant.** ρ varies by at most 25% across the cells (its coefficient of variation): one factor carries a laptop number to the phone.

The report states, per metric: honest (both hold, with the factor), shape only (slopes the same, ratio not constant), not honest (slopes differ), or too noisy (an interval crosses the bound — more repeats, not a verdict). It prints every slope with its interval, so a reader sees how wide the evidence is. On a matrix of 4 levels of N by 3 of V, a ratio that wanders by more than 25% widens the phone's intervals past the bound, so in practice it reads as too noisy rather than shape only.

What is expected to break which metric, and how the run sees it:

| Confound | Which metrics | How it shows | What the run does about it |
|---|---|---|---|
| All N nodes share the laptop's cores | time metrics at large N | host CPU high; latency rises with N in test L faster than in test P | cells above 70% host CPU are marked contended; work metrics are compared there instead of time metrics |
| Loopback has no latency and no loss | latency, sessions per record | test L latency far below test P at small N | variant L-net: `r` on the laptop's LAN address with a dummynet pipe to it, its delay and bandwidth set from the round trip and throughput measured to the phone in test P |
| The phone's cores are slower and throttle | `r`'s CPU, catch-up time | ρ for `r`'s CPU seconds per record well above 1 and rising with V | variant L-slow: `r` runs with background QoS (`taskpolicy -b`), which keeps it mostly on the efficiency cores and also throttles its disk I/O — an approximation of a slower core, not a reproduction; the pilot measures one fixed workload with and without it and records the ratio |
| The phone's radio sleeps between bursts | steady latency at low rates | test P latency has a floor test L lacks | reported, not corrected: it is the phone's behaviour, and a reason to keep test P for latency |
| Observing costs the phone more than the laptop | the phone's CPU | the process total grows faster with V than the node's threads | the node's threads are measured apart; the remainder is reported as the observation's cost |
| The application is suspended | everything on the phone | application state leaves active | the agent keeps the idle timer off; the cell is marked inactive |
| Low Power Mode lowers the phone's clocks | the phone's CPU and every time metric | the sample reports it on | refused at preflight; a cell in which it turns on is marked `low_power` and left out of every fit |
| Observing costs the laptop node too | `r`'s CPU on the laptop | the event route writes JSON inside the measured process | the pilot measures `r` with and without an open stream at V = 100,000; the difference is reported as the laptop's observation cost beside the phone's |

Test L runs plain first, then test P, then L-net and L-slow: L-net is sized from what test P measures, so it cannot come first. If L-net with L-slow is honest where plain L is not, the laptop is honest given those two knobs, and they become the default of every later laptop run.

## Running the matrix unattended

**Deadlines.** Nothing waits without a bound, and a bound that passes marks the cell and moves on:

| Wait | Bound | On expiry |
|---|---|---|
| quiet before a cell: every node under 5% of one core for 10 s | 120 s | the cell runs, marked `not_quiet` |
| thermal state nominal before a cell (test P) | 600 s | the cell runs, marked `thermal` |
| `r` holds every steady record | 60 s after the last write | `incomplete` |
| catch-up | the larger of 120 s and V / 100 seconds | `join_hung` when the join has not answered and the stream is silent for 30 s, else `incomplete` |
| fill | the larger of 300 s and V / 20 seconds | the level of V is skipped for that N, recorded |
| a node answers its status | 30 s | the cell is `incomplete`; the coordinator restarts that N |
| the phone's surface answers (test P) | 30 s | the coordinator relaunches the application over the cable (`xcrun devicectl device process launch`) and restarts that N once; a second failure marks the N `node_down`, as on the laptop |

**Its own processes.** The coordinator starts every node in a process group of its own and writes the group's id and every node's pid to the run directory as it starts them. A resumed run stops only the processes recorded under its run id, and still refuses any other `pdn-node-http` it finds.

**Resume.** Each cell appends its row to `load.csv` as it ends, with the run id. A coordinator started again with the same run id keeps every N whose cells are all in the table, moves the rows of an N it left unfinished to `load.discarded.csv`, and runs that N again from a fresh pod. A resumed N is therefore whole, and its pod's history is the same as an uninterrupted one's.

**Preflight**, before the first cell and refused on failure: no `pdn-node-http` process left from an earlier run (they are listed and the run stops rather than killing what it did not start); free disk at least twice the run's estimate (V × S × N per level); the release profile on every node and on the phone; in test P, the phone's test build packaging the workspace's commit and Low Power Mode off, refused otherwise, and a battery that is not charging reported — a phone off the laptop's Wi-Fi fails its first join, and that N is marked `node_down`; for L-net, no rule left in the coordinator's pf anchor and no dummynet pipe left at the pipe numbers it uses.

**Sleep.** The coordinator runs under `caffeinate -dims` and keeps the laptop on its charger; it compares its wall clock with its monotonic clock every second, and a jump above 5 s marks the cell `slept`.

**Cleanup.** `r`'s directory is deleted in every catch-up, after its leave; after each N, every node's directory. Free disk is recorded in `meta.json` per cell.

**L-net needs root.** dummynet and pf are configured by `dnctl` and `pfctl`, which need root. The coordinator runs them through `sudo -n` with rules in an anchor of its own (`sync-test`), so it touches nothing else in pf, flushes the anchor and deletes its pipes on exit, on interrupt and on start, and refuses to run L-net when `sudo -n` asks for a password: the person grants those two commands in sudoers once, or L-net is run attended. Loopback traffic to the laptop's own LAN address may never cross the rules, so L-net proves its pipe before it measures: a round trip to `r` through the pipe must show the pipe's delay, or L-net stops and says the shaping did not take. After a run killed with no chance to clean up, `sudo pfctl -a sync-test -F all && sudo dnctl -q flush` restores the laptop's network.

**How long it takes.** An estimate from the bounds, until the pilot at N = 2 and 4 replaces it. A cell takes 6 to 10 minutes — the quiet wait, idle (5.5 minutes in the first repeat), steady with its drain, the catch-up — so 84 cells of one variant take 8 to 14 hours, and the fills add up to an hour more at N = 2, where one writer fills 100,000 records alone. With 3 repeats each variant is a night and a day; with 2 repeats, 6 to 10 hours, one night. The pilot decides, and the choice is recorded in `meta.json`.

**The pilot's gates.** The matrix is not scheduled until the pilot at N = 2 has shown: zero `lagged` in an observed join of 100,000 records, since otherwise every catch-up at the top level of V is left out of the fit; L-net's pipe proven; L-slow's effect on one fixed workload recorded; the predicted pod-pass times matching the sessions seen while idle; and the laptop's observation cost measured.

## What the report looks like

Per cell, one row in `load.csv`: test, variant, N, V, S, repeat, the marks above, and for each workload the metrics above. Per metric, the fit and the verdict:

```
metric                slope N (L / P)          slope V (L / P)          ratio P/L   cv     at V=0   verdict
steady latency p50    0.42±0.05 / 0.45±0.08    0.08±0.03 / 0.11±0.05    3.1         0.12   2.9      honest, factor 3.1 (L-net)
r CPU s per record    0.05±0.04 / 0.06±0.06    0.31±0.04 / 0.62±0.07    2.0–4.4     0.41   1.8      not honest: V slope differs
catch-up records/s    −0.02±0.05 / −0.01±0.30  −0.18±0.04 / −0.20±0.28  0.38        0.31   -        too noisy
sessions per record   0.97±0.03 / 0.95±0.04    0.00±0.02 / 0.01±0.02    1.0         0.03   1.0      honest
```

The numbers are the format, not a measurement. Beside the table: the cells left out of each fit and why, and the observation's cost on the phone per cell.

## What it does not answer

- Relay paths and NAT: the phone and the laptop share one Wi-Fi network, direct paths only.
- Several phones: one phone, as `r` or as the joiner.
- Battery: the phone is on its cable, which charges it.
- Loss: whether every record arrives is `design.md`'s question. A load cell whose `r` never holds a record is marked incomplete and left out of the fits, but it is not classified.

## What it is built on

The `sync-observation` change: the event route with eager stamping, the observed join, the pod-interval setting and the record view in `pdn-node-http` behind its `observation` feature; the same routes in the phone's test build, served on its loopback, with the facade's runtime threads named, the per-thread CPU sample, the idle timer kept off and the application state reported. Laptop CPU needs nothing: `ps` reads it.

## One command

`just load`, run from the workspace, collects everything a full report reads for test L on the laptop: the ramp. It grows one pod per N at a steady 20 records a second, 256 B each, from empty to 20,000 records, for N = 2, 4, 8, 16, 12 and 24, and writes a row every 100 records up to 500 and every 500 after that into `ramp.csv`. A pod stops early when its reader holds no new content for 60 s or its median falls more than 30 s behind, and a stop keeps the reader's last events and listing under `stalled/`; a reader that lost events (`lagged` in the row) is read by its listing first, as in the catch-up, and holds everything or stalls. It also stops when a writer drops out — ten refusals in a row or its process gone — marked `writes_failed`, with the writers' logs and the refusals under `writes-failed/`; a lone refusal only counts in the row's `write_failures`. An N whose ramp raises — a node gone mid-step — goes to `ramp_errors` in `meta.json`, and the next N runs. A pod that reaches its size is then caught up: its reader is wiped and joins again, and the time it takes to hold every record once more goes into `meta.json`, marked incomplete when it takes longer than 120 s or a second per 100 records, whichever is more.

The command first builds the node binary of the checked-out commit. The run lands in the workspace's `tmp/load` as `<run>-ramp-plain`, where `<run>` is `load-<date>-<time>` unless `--run` names it. On the laptop it takes about 2 hours: 17 minutes of ramp per N and a few of catch-up.

`just load` of `pdn-app` collects test P the same way, with the phone as `r`, into `<run>-ramp-phone`: it builds and installs the test build (`phone.py build`), runs `phone.py check`, and runs the ramp through this directory's `full.sh`. It takes about 2 hours too, after the one-time setup "Running test P" describes.

`just load --container` runs test L in the runner image instead, as CI does, into `sync-test/runs`; the images come first (`container.sh build`). Test P does not run in a container, because it needs the phone on the laptop's cable. `RAMP` in the environment replaces the ramp's `load.py` arguments, which is what a smoke run of the command itself needs.

## Running in a container

`container.sh build` builds the observation image of pdn-node-http from the workspace and, on top of it, the runner image; `container.sh run <load.py arguments>` runs the test in one container, every node a process inside it, results in `sync-test/runs/<run id>/`. The nodes' stores stay on the container's own disk, since a mounted host directory would slow the store and the numbers with it. `CPUS` and `MEMORY` cap the container (`docker run --cpus/--memory`); on Linux the runner reads the cap from the cgroup and measures the laptop-busy share against it rather than against the host's cores, reads each node's CPU time from `/proc` at 10 ms resolution, and records the CPU model and the hypervisor's steal time at the start and the end of the run in `meta.json`. `docker stop` ends the run cleanly: the runner stops every node before the container exits.

## Running test P

Test P needs the phone on the laptop's USB cable and on the laptop's Wi-Fi, and nothing from a person while it runs. Once per machine and signing profile, a person signs Xcode into the account that signs the application, trusts the developer profile on the phone, sets Auto-Lock to Never — a locked phone refuses a launch, and the agent keeps the screen on only from its launch on — and grants the application's Local Network permission at its first launch. A free profile lives 7 days; `phone.py build` signs anew. `brew install libimobiledevice` provides `iproxy` and `idevice_id`.

`python3 -I pdn-app/sync-test/phone.py build` packages the facade's test build of the checked-out commit (`pdn-sdk`'s `just test-build`) and builds the application's test build against it in the release configuration: the facade goes under `modules/pdn/ios/test/` and is linked through `PDN_TEST_BUILD=1` at `pod install`, the bundle is built with `EXPO_PUBLIC_PDN_TEST_BUILD=1`, the build lands in `ios/dd-test`, and the result is installed on the phone. The release facade beside it is never touched, and the pods are put back on it whatever happens. `phone.py check` drives the installed build through a stub coordinator and one laptop node; it is run with Auto-Lock at 30 seconds, so its 60-second idle shows the agent keeping the screen on, and `phone.py release-check` builds the release and finds nothing of the agent in it.

`python3 -I sync-test/load.py --variant phone <matrix>` then runs the matrix with the phone as `r`. It refuses a test build of a commit other than the workspace's, opens a usbmux tunnel (`iproxy`) from a free port of the laptop's loopback to the surface on the phone's, and launches the application with the run id and the surface's port as its launch environment (`xcrun devicectl device process launch --terminate-existing`). The phone's node is then driven through the surface as a laptop node is: a reset brings up each incarnation of `r` on an empty directory at the run's pod interval on direct paths, and before each one the clock offset is measured through the tunnel from the shortest of 20 round trips — 2 to 3 ms over the cable. The laptop nodes' endpoints bind every interface, since the phone dials them at the local network addresses their invites carry. The device samples are read every second into `phone.jsonl`; `r`'s CPU and memory come from them, and they mark a cell `inactive`, `low_power` or `thermal`. The rows carry `test` P and `variant` phone, and `analyze.py <L run>/load.csv <P run>/load.csv` puts the two tests side by side. `sh pdn-app/sync-test/compare.sh <id> <matrix>` runs test P and then test L on one matrix as `<id>-phone` and `<id>-plain`, resuming either after a stop, and writes the verdict to `tmp/load/<id>.analysis.txt`.

## Running on CI

`.github/workflows/load.yml` runs test L by hand (`workflow_dispatch`) on a GitHub-hosted runner of 4 cores, on the ref it is started from: it builds the node image with the layer cache and the runner image over it (`container.sh runner`), and runs `just load --container` ("One command") in the runner image. `publish.sh` then writes the run's report (`report.py`). It leads with how each N's ramp ended — reached, stalled, behind, writes failed or error, the records it reached, its write failures and lost events, its catch-up done, incomplete or hung, lagged when its end came from the joiner's listing, and where a stop's evidence is — and then gives each metric per N beside its baseline, the latest earlier run of the same profile: the same test, rate, size, N levels, pod interval, CPU budget and catch-up, so a run with other arguments starts a history of its own. The metrics are the delay from the write to the record readable on `r`, median and 95th percentile, at 1,000, 5,000, 10,000 and 20,000 records, each from the first step at or past that size, since a failed write shifts every later step off the grid; `r`'s CPU and the sessions per record, the median over the ramp's steps; `r`'s content lag, from a record's insert to its content readable on `r`'s own clock, median and 90th percentile; and the catch-up's time and rate, which a lagged catch-up does not give: polled listings sit in its time, so it is neither compared nor a baseline. An N whose stop or catch-up changed is flagged whatever its numbers, and a metric is flagged when it moved past ±30 % for a time metric or ±15 % for a work or events metric — the shared runner moves time and barely moves work. One ramp per N gives no repeats to tell a change from the runner's noise, so a flag is a lead to look at, not a verdict. The report goes to the job's summary, and with `publish` on the run's `summary.json`, `report.md` and `ramp.csv` go to `runs/<run id>/` of the `load-history` branch beside `index.html`, which the same step rebuilds from every run on the branch: the page GitHub Pages serves, a chart per metric with a line per N and a point per day, the day's last run, each point naming its day, run and mee-pdn commit and opening that commit. The run stays an artifact of the job for 14 days, with its events, its logs and a stall's evidence. Nothing fails the job on a flagged change: the thresholds are first to be calibrated on the history.
