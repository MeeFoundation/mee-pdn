# What a load run on CI leaves behind

These are the outputs of `.github/workflows/load.yml` (`load.md`, "Running on CI") for 5 runs, built by `report.py` in the order the workflow would have run them, each run's `index.html` rebuild included.

`local-full` is a run of the workflow's default profile, `--grid --n 2,4,8,16 --v 0,1000,10000 --s 256 --repeats 3`, which took 44 minutes under 4 CPUs. `local-a`, `local-b` and `local-c` are runs of a narrower profile, `--grid --n 2,4 --v 0,1000 --s 256 --repeats 3`, about 10 minutes each, kept here because three runs of one profile on one day show the comparisons and the day's point. All 4 are real runs, made on a laptop in Docker under `CPUS=4` from one node image. That image predates the rebase onto main, so their numbers describe the platform's behavior on that image, not the current code; their mee-pdn revision, `14ad3c5+dirty`, links to the commit the image was built from, which is not on GitHub yet.

`example-regression` is not a run of the product. It is `local-c` moved a day later, with N = 4 made worse by hand, to show a regression the report flags: sessions per record ×1.6, r CPU per record ×1.5, catch-up time ×3, and repeat 2 of N = 4 V = 1,000 marked `fill_stalled`. Its mee-pdn revision reads `example`, so it links nowhere and cannot pass for a real one.

## Where each output appears

1. **The job's summary**, on the run's page in GitHub Actions. It is the run's `report.md`; `job-summary.md` is the one `example-regression` shows. It lists the cells the platform did not finish first, then the changes past the threshold against the baseline, then every metric per cell with its change in brackets. The mee-pdn revision of the run and of its baseline links to the commit.
2. **The job's artifact**, named after the run id and kept for 14 days: the whole run directory, 23 MB for the narrow profile. It holds `meta.json`, `load.csv`, `cpu.jsonl` (each node's CPU every second), `pids.jsonl`, `events/` (each node's event stream, gzipped) and `logs/` (each node's log), plus `stalled/` or `hung/` with a stack, the last events and the log of a cell that stalled. Nothing here copies it: it is what a flagged change gets diagnosed from.
3. **The `load-history` branch**, which GitHub Pages serves. `load-history/` is that branch after the 5 runs: `index.html` and, per run, `runs/<run id>/summary.json`, `report.md` and `load.csv`. Every run rebuilds `index.html` from all of them and pushes it, so the page needs no hand. Open `load-history/index.html` in a browser.

## The history page

One section per profile, newest first: a table of every run, then a chart per metric, a line per cell. A point is the last run of its UTC day: `local-a`, `local-b` and `local-c` all ran on 2026-10-14, so that day's point is `local-c`, and the table marks the other two "a later run that day". Hovering a point names its day, the cell's value, the run and the mee-pdn commit; clicking it opens the commit. "Values by day" under each chart lists the same points as a table, with the commit and the run's report linked.

`local-full`'s section splits its 12 cells by N, since more than 8 lines would run past the palette. It shows what the wider profile meets on 4 cores and on that image: at N = 8 and 16 every repeat with an empty pod is `contended`, so the time metrics of those cells are left out and only their work metrics are charted; every N = 4, 8 and 16 repeat at V = 1,000 is `incomplete`; and every pod of N = 4 and above stalled while filling to V = 10,000, which the report lists first as cells the platform did not finish.

## The comparisons in this history

| Run | Baseline | Flagged |
|---|---|---|
| `local-a` | none, the first run of its profile | nothing to compare with |
| `local-b` | `local-a` | N = 2 V = 1,000 finished 1 of 3 repeats instead of 3 |
| `local-c` | `local-b` | N = 2 V = 1,000 finished all 3 again |
| `example-regression` | `local-c` | sessions per record +60 % at N = 4 V = 0 and +57 % at N = 4 V = 1,000; catch-up time +328 % and catch-up rate −76 % at N = 4 V = 1,000; that cell finished 2 of 3 repeats |
| `local-full` | none, the first run of its profile | nothing to compare with |

The baseline is the latest earlier run of the same profile, whatever its day: the day's point is the charts' rule, not the report's.

The three real runs of one profile flag no metric, although their medians of catch-up time and idle CPU move by more than 30 % from one run to the next: a metric is flagged only when both runs measured it in 2 or more usable repeats whose ranges do not overlap. They do flag the one cell, N = 2 V = 1,000, whose repeats sometimes miss a deadline (`incomplete`: `r` did not hold every record in time); the flag recurs in every run where that cell's count of finished repeats changes.

The same rule has a price, visible in `example-regression`: every repeat's r CPU per record at N = 4 is 1.5 times `local-c`'s, and none of it is flagged, because the 3 repeats of each run overlap. With 3 repeats on a shared runner, a change of that size shows on the history page as a step in the line, not as a flag in the report.
