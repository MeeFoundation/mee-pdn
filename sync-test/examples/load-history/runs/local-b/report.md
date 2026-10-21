# Load run `local-b`

- mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), pdn-app `ad5211f+dirty`
- Profile: L plain · grid · N 2, 4 · V 0, 1,000 · S 256 B · 3 repeats · pod interval 300 s · 4 CPUs
- Started 2026-10-14 11:03 UTC, took 12 min
- Compared with `local-a` (mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), 2026-10-14 10:53 UTC)

## Cells the platform did not finish

None.

## Changes past the threshold

A cell whose count of finished repeats changed; a metric past ±30 % (time) or ±15 % (work, events) whose usable repeats, 2 or more in each run, do not overlap.

- **status** N=2 V=1,000 S=256 B: finished 3/3 → 1/3

## Metrics

The median over the repeats a metric can use; in brackets the change against the baseline.

### S = 256 B

| N | V | finished | steady p50, s | steady p90, s | steady p99, s | catch-up, s | catch-up, records/s | r CPU, s/record | sessions/record | idle r CPU, % | r RSS, MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 0 | 3/3 | 0.0431 (+2 %) | 0.0463 (-1 %) | 0.0502 (-1 %) | 0.102 (+14 %) | 3,621 (-1 %) | 0.0151 (+7 %) | 1 (+0 %) | 0.181 (-9 %) | 38.8 (+10 %) |
| 2 | 1,000 | 1/3 | 0.048 (-4 %) | 0.0538 (+1 %) | 0.122 (+109 %) | 0.386 (+64 %) | 3,112 (-46 %) | 0.0195 (+2 %) | 0.99 (-1 %) | 0.2 (+10 %) | 41.0 (+2 %) |
| 4 | 0 | 3/3 | 0.0753 (+18 %) | 0.094 (-11 %) | 0.116 (-0 %) | 0.0816 (-32 %) | 3,388 (+13 %) | 0.0137 (-2 %) | 2.97 (+1 %) | 0.298 (+200 %) | 35.1 (-0 %) |
| 4 | 1,000 | 3/3 | 0.0586 (-30 %) | 0.127 (-4 %) | 0.143 (+4 %) | 0.521 (+110 %) | 2,687 (-52 %) | 0.0239 (-8 %) | 2.99 (+4 %) | 0.271 (+36 %) | 43.9 (-1 %) |
