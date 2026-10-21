# Load run `local-c`

- mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), pdn-app `ad5211f+dirty`
- Profile: L plain · grid · N 2, 4 · V 0, 1,000 · S 256 B · 3 repeats · pod interval 300 s · 4 CPUs
- Started 2026-10-14 11:43 UTC, took 10 min
- Compared with `local-b` (mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), 2026-10-14 11:03 UTC)

## Cells the platform did not finish

None.

## Changes past the threshold

A cell whose count of finished repeats changed; a metric past ±30 % (time) or ±15 % (work, events) whose usable repeats, 2 or more in each run, do not overlap.

- **status** N=2 V=1,000 S=256 B: finished 1/3 → 3/3

## Metrics

The median over the repeats a metric can use; in brackets the change against the baseline.

### S = 256 B

| N | V | finished | steady p50, s | steady p90, s | steady p99, s | catch-up, s | catch-up, records/s | r CPU, s/record | sessions/record | idle r CPU, % | r RSS, MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 0 | 3/3 | 0.0432 (+0 %) | 0.046 (-1 %) | 0.0487 (-3 %) | 0.0947 (-7 %) | 3,639 (+0 %) | 0.0138 (-9 %) | 1 (+0 %) | 0.181 (-0 %) | 35.1 (-10 %) |
| 2 | 1,000 | 3/3 | 0.0501 (+4 %) | 0.0518 (-4 %) | 0.0548 (-55 %) | 0.483 (+25 %) | 2,899 (-7 %) | 0.0187 (-4 %) | 1 (+1 %) | 0.181 (-9 %) | 41.9 (+2 %) |
| 4 | 0 | 3/3 | 0.06 (-20 %) | 0.0885 (-6 %) | 0.113 (-3 %) | 0.093 (+14 %) | 2,979 (-12 %) | 0.0134 (-2 %) | 2.98 (+1 %) | 0.181 (-39 %) | 35.4 (+1 %) |
| 4 | 1,000 | 3/3 | 0.0972 (+66 %) | 0.141 (+10 %) | 0.16 (+12 %) | 0.291 (-44 %) | 5,505 (+105 %) | 0.0228 (-4 %) | 2.79 (-7 %) | 0.271 (-0 %) | 39.5 (-10 %) |
