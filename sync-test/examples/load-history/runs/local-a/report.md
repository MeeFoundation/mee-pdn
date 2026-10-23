# Load run `local-a`

- mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), pdn-app `ad5211f+dirty`
- Profile: L plain · grid · N 2, 4 · V 0, 1,000 · S 256 B · 3 repeats · pod interval 300 s · 4 CPUs
- Started 2026-10-14 10:53 UTC, took 10 min
- No earlier run of this profile: nothing to compare with

## Cells the platform did not finish

None.

## Metrics

The median over the repeats a metric can use.

### S = 256 B

| N | V | finished | steady p50, s | steady p90, s | steady p99, s | catch-up, s | catch-up, records/s | r CPU, s/record | sessions/record | idle r CPU, % | r RSS, MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 0 | 3/3 | 0.0425 | 0.0466 | 0.051 | 0.0896 | 3,655 | 0.0141 | 1 | 0.199 | 35.2 |
| 2 | 1,000 | 3/3 | 0.0503 | 0.0532 | 0.0584 | 0.235 | 5,756 | 0.0192 | 1 | 0.181 | 40.2 |
| 4 | 0 | 3/3 | 0.0639 | 0.106 | 0.116 | 0.12 | 2,987 | 0.014 | 2.94 | 0.0994 | 35.3 |
| 4 | 1,000 | 3/3 | 0.0838 | 0.133 | 0.137 | 0.248 | 5,608 | 0.026 | 2.88 | 0.199 | 44.3 |
