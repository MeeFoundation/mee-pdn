# Load run `ci-20261022-1-1-ramp-plain`

- mee-pdn [`fe5d901`](https://github.com/MeeFoundation/mee-pdn/commit/fe5d90106b9413edd3473cb8cd0b1ded42272f98); [CI run](https://github.com/MeeFoundation/mee-pdn/actions/runs/39821925552)
- Profile: L plain · 20 records/s up to 20,000 · N 2, 4, 8, 12, 16, 24 · pod interval 300 s · 4 CPUs · catch-up
- Started 2026-10-22 17:26 UTC, took 128 min
- No earlier run of this profile: nothing to compare with

## Each N's ramp

| N | stop | records reached | write failures | events lost | catch-up | host busy, max % | evidence |
|---|---|---|---|---|---|---|---|
| 2 | reached | 20,000 | 0 | 0 | done (lagged) | 26.9 | – |
| 4 | reached | 20,000 | 0 | 0 | done (lagged) | 85.5 | – |
| 8 | reached | 20,000 | 0 | 0 | done (lagged) | 98.5 | – |
| 12 | reached | 20,000 | 0 | 0 | done (lagged) | 98.7 | – |
| 16 | reached | 20,000 | 0 | 0 | done (lagged) | 98.8 | – |
| 24 | reached | 20,000 | 0 | 0 | done (lagged) | 98.8 | – |

## Metrics

Delay is from the write on a writer to the record readable on r, the median and the 95th percentile of the ramp's step that ends at that many records. r CPU and sessions per record are the median over the steps. Content lag is r's own clock from a record's insert to its content readable, over the whole ramp.

### Delay

| N | p50 at 1,000, ms | p95 at 1,000, ms | p50 at 5,000, ms | p95 at 5,000, ms | p50 at 10,000, ms | p95 at 10,000, ms | p50 at 20,000, ms | p95 at 20,000, ms |
|---|---|---|---|---|---|---|---|---|
| 2 | 9.59 | 32.9 | 27.4 | 59.7 | 70.6 | 110 | 184 | 252 |
| 4 | 20.3 | 113 | 71.2 | 114 | 184 | 298 | 453 | 734 |
| 8 | 206 | 345 | 566 | 958 | 1,095 | 1,869 | 2,142 | 3,951 |
| 12 | 521 | 848 | 1,549 | 2,518 | 2,459 | 3,966 | 5,366 | 8,435 |
| 16 | 1,050 | 1,797 | 2,878 | 4,643 | 5,289 | 8,270 | 8,914 | 14,206 |
| 24 | 2,592 | 4,096 | 6,255 | 9,399 | 10,896 | 16,977 | 18,848 | 28,520 |

### Work, content lag and catch-up

| N | r CPU, ms/record | sessions/record | r content lag p50, ms | r content lag p90, ms |
|---|---|---|---|---|
| 2 | 22.5 | 0.809 | 0.571 | 0.969 |
| 4 | 42.0 | 1.93 | 2 | 5.67 |
| 8 | 25.6 | 1.39 | 5.52 | 16.1 |
| 12 | 18.4 | 1.19 | 7.35 | 22.5 |
| 16 | 14.5 | 1.07 | 9.84 | 31.1 |
| 24 | 12.6 | 1.23 | 16.5 | 64.4 |
