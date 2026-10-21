# Load run `example-regression`

- mee-pdn `example`, pdn-app `ad5211f+dirty`
- Profile: L plain · grid · N 2, 4 · V 0, 1,000 · S 256 B · 3 repeats · pod interval 300 s · 4 CPUs
- Started 2026-10-15 11:43 UTC, took 10 min
- Compared with `local-c` (mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), 2026-10-14 11:43 UTC)

## Cells the platform did not finish

| N | V | S | repeat | mark | evidence |
|---|---|---|---|---|---|
| 4 | 1,000 | 256 | 2 | fill_stalled | stalled/L-plain-N4-V1000-S256-fill |

## Changes past the threshold

A cell whose count of finished repeats changed; a metric past ±30 % (time) or ±15 % (work, events) whose usable repeats, 2 or more in each run, do not overlap.

- **worse** sessions/record, N=4 V=0 S=256 B: 2.98 → 4.78 (+60 %; threshold ±15 %)
- **status** N=4 V=1,000 S=256 B: finished 3/3 → 2/3
- **worse** catch-up, s, N=4 V=1,000 S=256 B: 0.291 → 1.24 (+328 %; threshold ±30 %)
- **worse** catch-up, records/s, N=4 V=1,000 S=256 B: 5,505 → 1,318 (-76 %; threshold ±30 %)
- **worse** sessions/record, N=4 V=1,000 S=256 B: 2.79 → 4.4 (+57 %; threshold ±15 %)

## Metrics

The median over the repeats a metric can use; in brackets the change against the baseline.

### S = 256 B

| N | V | finished | steady p50, s | steady p90, s | steady p99, s | catch-up, s | catch-up, records/s | r CPU, s/record | sessions/record | idle r CPU, % | r RSS, MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 0 | 3/3 | 0.0432 (+0 %) | 0.046 (+0 %) | 0.0487 (+0 %) | 0.0947 (+0 %) | 3,639 (+0 %) | 0.0138 (+0 %) | 1 (+0 %) | 0.181 (+0 %) | 35.1 (+0 %) |
| 2 | 1,000 | 3/3 | 0.0501 (+0 %) | 0.0518 (+0 %) | 0.0548 (+0 %) | 0.483 (+0 %) | 2,899 (+0 %) | 0.0187 (+0 %) | 1 (+0 %) | 0.181 (+0 %) | 41.9 (+0 %) |
| 4 | 0 | 3/3 | 0.06 (+0 %) | 0.0885 (+0 %) | 0.113 (+0 %) | 0.279 (+200 %) | 993 (-67 %) | 0.0201 (+50 %) | 4.78 (+60 %) | 0.181 (+0 %) | 35.4 (+0 %) |
| 4 | 1,000 | 2/3 | 0.102 (+4 %) | 0.135 (-4 %) | 0.237 (+48 %) | 1.24 (+328 %) | 1,318 (-76 %) | 0.0297 (+30 %) | 4.4 (+57 %) | 0.316 (+17 %) | 39.4 (-0 %) |
