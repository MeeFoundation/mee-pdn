# Running the pod load test

How to run test L (the laptop) and test P (the phone) and what each knob does. What the test measures and why is in [`load.md`](load.md); this page is the operator's card.

## In one paragraph

`just load` grows one pod per N at a steady rate until it holds 20,000 records, then wipes the reader `r` and lets it join again, so one run yields the write-to-read delay at every size and one catch-up per N. Every node is a `pdn-node-http` process of the checked-out commit with the observation routes on; the coordinator is `load.py`, which drives the nodes over HTTP on loopback. The result is a directory with `ramp.csv`, `meta.json`, the events and the nodes' logs, and `report.py` turns it into `report.md` against the latest earlier run of the same profile.

## Commands

| Where | Command | Takes | Lands in |
|---|---|---|---|
| Laptop | `just load [--profile <p>] [--run <id>] [-- <load.py args>]` | about 2 hours: a release build, then 17 minutes of ramp and a few of catch-up per N | `tmp/load/<id>-<p>-plain/` |
| Container, as CI does | `sh sync-test/container.sh build` once, then `just load --container [--cpus 4] [--memory 8g] [--profile <p>] [--run <id>] [-- <load.py args>]` | the same, on a known CPU budget | `sync-test/runs/<id>-<p>-plain/` |
| CI | GitHub, Actions, `load`, "Run workflow" on a ref, with the inputs below | up to 6 hours including the image build; one run at a time | the job's summary, a 14-day artifact, and with `publish` on the `load-history` branch and its GitHub Pages site |
| Phone, test P | in `pdn-app`: `just load [--profile <p>] [--run <id>] [-- <load.py args>]`, the phone on the cable after the one-time setup in `load.md`, "Running test P" | about 2 hours | `tmp/load/<id>-<p>-phone/` |
| Report by hand | `sh sync-test/publish.sh <run dir>`; add `push` to put it on the history branch | seconds | `report.md` in the run directory |
| Smoke | `just load --profile smoke` | about 5 minutes | as above |

`<id>` defaults to `load-<date>-<time>`; `<p>` is the profile, `ci` by default. A run id that already exists is resumed, not restarted. Everything a run is shaped by is an argument of `just load`; nothing is set in the environment before it.

## Profiles

A profile is a file of `load.py` arguments in `sync-test/profiles/`, one option per line, `#` comments allowed; `--profile` names it without the `.args`:

| Profile | Arguments | For |
|---|---|---|
| `ci` | `--ramp 20 --n 2,4,8,16,12,24 --v 20000 --s 256` | the run CI makes and the history compares; frozen, since a change here is a new baseline |
| `smoke` | `--ramp 20 --n 2,4 --v 2000 --s 256` | the command itself, in about 5 minutes |
| `pilot-100k` | `--ramp 20 --n 4 --v 100000 --s 256` | the 100,000-record cell of the pilot, by hand |

Arguments after `--` come after the profile's, and `load.py` takes the last value of a repeated option, so they override it: `just load --profile smoke -- --n 100` ramps 100 nodes to 2,000 records. A new shape that will be run more than once gets a file of its own.

```sh
just load --profile smoke                                  # does the command work
just load --profile smoke --run n100 -- --n 100 --v 2000   # 100 nodes, 2,000 records, into tmp/load/n100-smoke-plain
just load --container --cpus 4 --memory 8g                 # the CI run on the laptop's daemon, on CI's budget
```

The report compares a run only with earlier runs of the same profile: the same test, rate, payload size, N levels, pod interval, CPU budget and catch-up, read from the run's `meta.json`, not from its name. A run with other arguments starts a history of its own, so a changed `ci` profile means a new baseline, not a regression.

## `load.py` arguments

The full list is `python3 -I sync-test/load.py --help`; these are the ones a run is shaped by.

| Argument | Meaning | Default |
|---|---|---|
| `--ramp RATE` | ramp mode: one pod per N, filled at `RATE` records a second up to the largest `--v`, a row every 100 records up to 500 and every 500 after | off: the N × V × S matrix of `load.md` instead |
| `--n` | the N levels, member nodes with `r` included, comma-separated; run in that order | `2,4,8,16` |
| `--v` | the V levels, records the pod holds; in ramp mode its largest value is the ramp's end | `0,1000,10000,100000` |
| `--s` | payload sizes in bytes | `256,65536` |
| `--repeats` | repeats per matrix cell | `3` |
| `--pod-interval` | the nodes' pod reconcile interval, seconds; a pass inside a cell marks it `pod_pass` | `300`, the runtime's own |
| `--skip-catchup` | ramp mode: stop after the ramp, no wipe-and-rejoin of `r` | off |
| `--grid` | the matrix with short windows (idle 10 s, steady 20 s, catch-up from 45 s), for a quick N × V map | off |
| `--obs-cost` | after each pod's last level, `r`'s CPU with and without its event stream | off |
| `--variant plain\|phone` | test L, or test P with the phone as `r` (`pdn-app/sync-test/phone.py`) | `plain` |
| `--run-id`, `--out`, `--nodes-dir`, `--bin` | the run's name and resume, where runs and the nodes' stores go, the node binary | `load-<date>-<time>`, `tmp/load`, `nodes/` in the run, the release build |

## `just load` options and the workflow's inputs

| `just load` | Workflow input | Meaning |
|---|---|---|
| `--profile <p>` | `profile` (`ci`, `smoke`, `pilot-100k`) | the profile above; `ci` by default |
| `-- <args>` | `extra` | `load.py` arguments after the profile's, overriding it |
| `--run <id>` | set by the job: `ci-<date>-<run>-<attempt>` | the run's name; an existing one is resumed |
| `--container` | always on | run in the runner image, into `sync-test/runs` |
| `--cpus <n>`, `--memory <size>` | not set: the runner's 4 cores | `docker run --cpus` and `--memory`; the runner measures the host-busy share against the cap and records it in `meta.json` |
| | `publish` | add the run to the history branch and its page; off, the report still goes to the job's summary |

The container is named after the run, so `docker stop <id>-<p>-plain` ends it cleanly. `publish.sh` reads `HISTORY_BRANCH` (`load-history`) and `container.sh` reads `IMAGE` (`pdn-load-test`) from the environment; neither shapes a run.

## What a run stops on, and what is not a knob

A pod's ramp stops early when `r` holds no new content for 60 s, when the median delay falls more than 30 s behind, or when a writer drops out (ten refusals in a row, counted across steps, or its process gone), each marked in `ramp.csv` with the evidence kept under `stalled/` or `writes-failed/`. The waits, their bounds and the marks a cell can carry (`contended` above 70% host CPU, `lagged`, `incomplete`, `join_hung`, `slept`, `not_quiet`, `pod_pass`) are the table in `load.md`, "Running the matrix unattended", and constants at the top of `load.py`. They define what a cell is, so they are not arguments: a run with other windows would not compare with any other.

## Reading the result

`ramp.csv` is one row per N and step: records held, median and 95th delay, `r`'s CPU per record, the host's busy share, sessions per record, and how the ramp ended. `report.md` leads with how each N ended, then every metric beside its baseline, flagging a time metric past ±30% and a work metric past ±15%; a flag is a lead to look at, not a verdict, since one ramp per N has no repeats. The history page, built by `publish.sh` from every run on `load-history`, draws a chart per metric with a line per N and a point per day, each point naming its run and mee-pdn commit.
