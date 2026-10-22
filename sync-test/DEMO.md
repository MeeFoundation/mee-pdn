# Pod sync demo: run-sheet

`demo.py` runs one pod of pdn-node-http nodes on this laptop and draws what happens, full-screen, from the nodes' own event streams: 3 writers and 1 reader `r`, a prefill of 1,000 records of 256 B, 45 s of live writes at 10 records a second, then `r` leaves, its directory is wiped, and it joins again and catches up. Without pauses for talking it takes about 2 minutes; the result screen stays up until you press Enter.

## Before the talk

- Python 3.12 or newer as `python3` (the scripts use its f-string syntax); stdlib only.
- Build the node once: `cargo build --release -p pdn-node-http --features observation` (from the workspace root).
- No other `pdn-node-http` may run: the demo refuses to start next to one (`pgrep -lx pdn-node-http` must print nothing), which also keeps it away from a measurement run.
- Terminal: dark background, a large font, at least 100 columns × 32 rows (the dashboard warns in yellow when it is smaller). Test it once on the projector.
- Rehearse once on the stage laptop: `python3 -I sync-test/demo.py --rehearse`.

## Commands

Run from the workspace root.

| What | Command |
|---|---|
| Live demo, for the talk | `python3 -I sync-test/demo.py --v 300` |
| Live, larger prefill | `python3 -I sync-test/demo.py` |
| Rehearsal, no nodes | `python3 -I sync-test/demo.py --rehearse` |
| Rehearsal of a join that hangs | `python3 -I sync-test/demo.py --rehearse=hang` |
| Shorter | add `--steady-s 20`; other knobs: `--n`, `--v`, `--rate`, `--no-color` |

Enter or `q` leaves the result screen; Ctrl-C leaves at any moment, and every node the demo started is stopped either way. A live run leaves its numbers in `tmp/load/demo-<timestamp>/`, laid out like `load.py`'s: `load.csv`, `meta.json`, `events/`, `logs/`, `cpu.jsonl`.

**Use `--v 300` for the talk.** The join deadlock is fixed in main, but the catch-up can still stop short: content downloads lock up under a burst (finding 1 of the deck). Checked live on 9 October with 4 nodes: at `--v 1000` the catch-up stopped at 1,027 of 1,450 records; at `--v 300` one run caught up 750 records in 1.02 s and one stopped short. Live latency was p50 175–336 ms, p99 460–770 ms. A catch-up that stops short turns yellow, "incomplete at the deadline", after 30 s: say it is finding 1 happening live.

## What each panel shows, and what to say

1. **Header.** The phases, left to right: Start nodes, Form pod, Prefill, Live writes, Catch-up, Result. The current one is yellow; a phase that failed is red. Under it, one sentence of what is happening now, and the seconds spent in this phase. *"Every box here is a real node process on this laptop, each with its own store, talking over the network stack as it would between devices."*
2. **Nodes.** One row per node: its name and role, its CPU as a share of one core (from `ps`), its memory, and how many records it holds. The green dot lights when the node receives an entry from a peer. *"w1 to w3 write; r only reads. Watch the dots: every write lights up every other node."*
3. **Live sync.** During the prefill: records placed and readable on `r`. During the live writes: records placed against records readable on `r`, the time from a record's write on a writer to readable on `r` over the last 10 seconds (p50 is the typical record, p90 the slow tenth), a sparkline of that p50 for each second of the run, and the sessions per record. *"The number to watch is the green one: with 4 nodes, a record written on one node is readable on another in about 200 to 300 milliseconds."*
4. **Catch-up.** `r` leaves the pod, its directory is deleted (a phone reset), and it joins again from an invite with nothing. The bar is the records `r` holds out of everything the pod holds, with records per second. *"A device that lost everything gets the whole pod back in about a second — or, if the bar stops short, we are watching the content stall the test found."*
5. **What is happening.** The last six events in words: a node started, a member joined or left, a record arriving on `r` with its latency, a session between two nodes with the entries it carried. It shows a sample, not every event.
6. **Result.** Latency p50/p90/p99 from write to readable on `r`, `r`'s CPU time per record, the catch-up time and rate, sessions per record, and one sentence of what they mean. The same numbers are in `load.csv` of the run directory.

## If something fails

- **The demo exits before drawing anything** with `preflight: pdn-node-http processes this run did not start` — a node is still running. If no measurement run is going (`pgrep -fl load.py` prints nothing), stop it with `pkill -x pdn-node-http` and start again; if one is, wait for it or use `--rehearse`. `no node binary` — build it (above).
- **A red STOPPED panel** — a node did not answer or a step failed; the panel says which, and the logs are in the run directory. Press Enter, then run `--rehearse` and say so: *"this is the same dashboard on recorded numbers."*
- **Catch-up shows "join did not answer" in red** — this is the join deadlock above, caught as a finding and not a crash: the demo waits 30 s for the join, checks 15 s more that the node is silent, saves the last events, the logs and a stack sample, and goes on to the result. *"This is exactly what the load test is for: it finds where the platform stops, and keeps the evidence for the fix."*
- **Catch-up shows "incomplete at the deadline" in yellow** — the join answered but `r` did not hold everything within 30 s; the result says so.
- **The screen looks broken** (wrapped lines, garbled characters) — enlarge the window or shrink the font; `--no-color` for a projector that washes out colours.
- **Anything else** — Ctrl-C, then `python3 -I sync-test/demo.py --rehearse`. The rehearsal draws the whole run from synthetic events sized on the pilot (sized on the 9 October run at 4 nodes: p50 about 250 ms, p99 about 800 ms, catch-up about 1,000 records a second); its header says REHEARSAL.
