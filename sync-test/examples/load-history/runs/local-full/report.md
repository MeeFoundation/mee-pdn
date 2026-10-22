# Load run `local-full`

- mee-pdn [`14ad3c5+dirty`](https://github.com/MeeFoundation/mee-pdn/commit/14ad3c5), pdn-app `ad5211f+dirty`
- Profile: L plain · grid · N 2, 4, 8, 16 · V 0, 1,000, 10,000 · S 256 B · 3 repeats · pod interval 300 s · 4 CPUs
- Started 2026-10-14 14:38 UTC, took 44 min
- No earlier run of this profile: nothing to compare with

## Cells the platform did not finish

| N | V | S | repeat | mark | evidence |
|---|---|---|---|---|---|
| 4 | 10,000 | 256 | 0 | fill_stalled | stalled/L-plain-N4-V10000-S256-fill |
| 4 | 10,000 | 256 | 1 | fill_stalled | stalled/L-plain-N4-V10000-S256-fill |
| 4 | 10,000 | 256 | 2 | fill_stalled | stalled/L-plain-N4-V10000-S256-fill |
| 8 | 10,000 | 256 | 0 | fill_stalled | stalled/L-plain-N8-V10000-S256-fill |
| 8 | 10,000 | 256 | 1 | fill_stalled | stalled/L-plain-N8-V10000-S256-fill |
| 8 | 10,000 | 256 | 2 | fill_stalled | stalled/L-plain-N8-V10000-S256-fill |
| 16 | 10,000 | 256 | 0 | fill_stalled | stalled/L-plain-N16-V10000-S256-fill |
| 16 | 10,000 | 256 | 1 | fill_stalled | stalled/L-plain-N16-V10000-S256-fill |
| 16 | 10,000 | 256 | 2 | fill_stalled | stalled/L-plain-N16-V10000-S256-fill |

## Metrics

The median over the repeats a metric can use.

### S = 256 B

| N | V | finished | steady p50, s | steady p90, s | steady p99, s | catch-up, s | catch-up, records/s | r CPU, s/record | sessions/record | idle r CPU, % | r RSS, MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 0 | 3/3 | 0.0433 | 0.0475 | 0.0516 | 0.0803 | 3,616 | 0.0145 | 1 | 0.199 | 35.4 |
| 2 | 1,000 | 3/3 | 0.0503 | 0.0533 | 0.0596 | 0.241 | 6,102 | 0.0187 | 1 | 0.181 | 40.7 |
| 2 | 10,000 | 2/3 | 0.0927 | 0.0999 | 0.144 | 5.67 | 2,003 | 0.0406 | 0.99 | 0.19 | 71.5 |
| 4 | 0 | 3/3 | 0.0694 | 0.106 | 0.119 | 0.0904 | 2,932 | 0.0149 | 2.93 | 0.362 | 35.2 |
| 4 | 1,000 | 0/3 | – | – | – | – | – | – | – | – | – |
| 4 | 10,000 | 0/3 | – | – | – | – | – | – | – | – | – |
| 8 | 0 | 3/3 | – | – | – | – | – | 0.0271 | 7.54 | 0.272 | 40.7 |
| 8 | 1,000 | 0/3 | – | – | – | – | – | – | – | – | – |
| 8 | 10,000 | 0/3 | – | – | – | – | – | – | – | – | – |
| 16 | 0 | 3/3 | – | – | – | – | – | 0.0216 | 6.21 | 0.272 | 44.1 |
| 16 | 1,000 | 0/3 | – | – | – | – | – | – | – | – | – |
| 16 | 10,000 | 0/3 | – | – | – | – | – | – | – | – | – |
