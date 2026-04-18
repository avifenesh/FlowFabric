# Tier-1 envelope collapse — POST-change measurement

Captured on `feat/tier1-envelope-collapse @ ef587da` immediately
after W1 landed the Tier-1 change. Same host, same Valkey process,
same kernel uptime band, same load band, same `[profile.release]`
codegen, same protocol as `tier1-pre.md`.

Conditions-match attestation is in `tier1-delta.md §1`.

## §1 Protocol

Identical to `tier1-pre.md §2`. Four binaries, all rebuilt from
source on this branch (including the ferriskey source with the
collapse inlined):

```
ferriskey-baseline-scenario1   benches/comparisons/ferriskey-baseline
baseline-scenario1             benches/comparisons/baseline
probe-ferriskey                benches/perf-invest/envelope-probe
probe-redis-rs                 benches/perf-invest/envelope-probe
```

`cargo tree -p ferriskey --no-default-features | md5sum` output is
`185f84004b477e1cc85eda2af4bb8e66` on both main@fcfb98c and
feat/tier1-envelope-collapse@ef587da — W3's attestation
independently verified.

Run sequence unchanged: FLUSHDB → sleep 2 → binary → next.
Alternation: warm-discard(fk) → warm-discard(rr) → fk1 rr1 fk2 rr2
fk3 rr3 fk4 rr4 fk5 rr5. Discard dropped; 5 real runs per workload
per client.

## §2 Scenario 1 — RPUSH / BLMPOP / INCR, 10 000 tasks × 16 workers

### ferriskey-baseline (post Tier-1)

| run | ops/sec | p99 ms | blmpop_outcomes          |
| --: | ------: | -----: | ------------------------ |
|  1  |  8079.7 |  0.37  | ok=10000 nil=15  err=0   |
|  2  |  8082.4 |  0.38  | ok=10000 nil=15  err=0   |
|  3  |  8066.3 |  0.37  | ok=10000 nil=15  err=0   |
|  4  |  8047.1 |  0.36  | ok=10000 nil=15  err=0   |
|  5  |  8118.9 |  0.37  | ok=10000 nil=15  err=0   |

- **median ops/sec: 8079.7**
- min 8047.1, max 8118.9
- spread (max-min)/median = **0.89 %**

### baseline (redis-rs 1.2.0) — control

| run | ops/sec | p99 ms | blmpop_outcomes          |
| --: | ------: | -----: | ------------------------ |
|  1  | 14810.7 |  0.38  | ok=10000 nil=0   err=15  |
|  2  | 14740.0 |  0.39  | ok=10000 nil=0   err=15  |
|  3  | 14808.5 |  0.35  | ok=10000 nil=0   err=15  |
|  4  | 14820.7 |  0.36  | ok=10000 nil=0   err=15  |
|  5  | 14706.9 |  0.39  | ok=10000 nil=0   err=15  |

- **median ops/sec: 14808.5**
- min 14706.9, max 14820.7
- spread (max-min)/median = **0.77 %**
- redis-rs control movement: **+0.18 %** vs pre median — inside pre
  spread (0.32 %), so the host state did not drift systematically.

### Post-change BLMPOP gap

ferriskey median 8079.7 / redis-rs median 14808.5 = **54.56 %**, a
**-45.44 %** deficit. Pre was -45.02 %. The gap moved -0.42 pp —
well inside either side's spread band.

## §3 Envelope probe — 1 worker × 100 000 iter INCR + 1 000 warmup

### probe-ferriskey (post Tier-1)

| run | p50 ms | p95 ms | p99 ms | p99.9 ms | mean ms |
| --: | -----: | -----: | -----: | -------: | ------: |
|  1  | 0.031  | 0.040  | 0.045  | 0.068    | 0.032   |
|  2  | 0.031  | 0.038  | 0.043  | 0.063    | 0.032   |
|  3  | 0.031  | 0.039  | 0.046  | 0.069    | 0.033   |
|  4  | 0.031  | 0.040  | 0.045  | 0.069    | 0.032   |
|  5  | 0.031  | 0.040  | 0.047  | 0.072    | 0.033   |

- median p50 = 0.031 ms  (spread 0.0 % — all five runs land on the
  same integer microsecond)
- median p95 = 0.040 ms  (spread 5.0 %)
- median p99 = 0.045 ms  (spread 8.9 %)
- median p99.9 = 0.069 ms  (spread 13.0 %)
- median mean = 0.032 ms  (spread 3.1 %)

### probe-redis-rs — control

| run | p50 ms | p95 ms | p99 ms | p99.9 ms | mean ms |
| --: | -----: | -----: | -----: | -------: | ------: |
|  1  | 0.030  | 0.034  | 0.042  | 0.065    | 0.031   |
|  2  | 0.030  | 0.033  | 0.041  | 0.056    | 0.031   |
|  3  | 0.031  | 0.034  | 0.042  | 0.057    | 0.031   |
|  4  | 0.031  | 0.034  | 0.042  | 0.059    | 0.031   |
|  5  | 0.031  | 0.038  | 0.045  | 0.063    | 0.032   |

- median p50 = 0.031 ms  (spread 3.2 %)
- median p95 = 0.034 ms  (spread 14.7 %)
- median p99 = 0.042 ms  (spread 9.5 %)
- median p99.9 = 0.059 ms  (spread 15.3 %)
- median mean = 0.031 ms  (spread 3.2 %)

## §4 Raw headline summary

| workload / metric                  |   pre median |  post median |
| ---------------------------------- | -----------: | -----------: |
| ferriskey-baseline scenario-1 ops  |       8127.6 |       8079.7 |
| baseline scenario-1 ops            |      14782.6 |      14808.5 |
| probe-ferriskey p50 ms             |        0.031 |        0.031 |
| probe-ferriskey p99 ms             |        0.044 |        0.045 |
| probe-ferriskey p99.9 ms           |        0.068 |        0.069 |
| probe-ferriskey mean ms            |        0.032 |        0.032 |

Full pre/post comparison + delta-vs-noise analysis in
`tier1-delta.md`.
