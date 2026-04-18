# Tier-1 envelope collapse — PRE-change baseline

Captured on `main @ fcfb98c` immediately before W1 landed
`feat/tier1-envelope-collapse @ ef587da`. Paired with
`tier1-post.md` + `tier1-delta.md` on the feat branch. Same host,
same Valkey, same session — no reboot or scheduler shuffle between
pre and post.

## §1 Host + environment

```
cpu        AMD EPYC 9R14
cores      16
mem        30 GiB total / 22 GiB available
kernel     Linux 6.8.0-1031-aws (Ubuntu SMP)
kernel_up  241d 12:49 (20 868 579 s at pre start)
valkey     redis_version 7.2.4 / valkey_version 7.2.12 (uptime 1 071 017 s at pre start)
load_avg   4.55 / 5.32 / 5.06  at pre start
git_sha    fcfb98c (main)
```

Load average of 4.55 is recorded for honesty — the host is shared
and not idle. This applies to pre AND post (same session, same load
band throughout); it raises the noise floor for both sides
symmetrically, which is the point of the paired design.

## §2 Protocol

Four binaries, all built `cargo build --release`, profile
`[profile.release] opt-level=3 lto=fat codegen-units=1 debug=0` —
the profile operators ship with. No feature flips, no env
overrides.

```
ferriskey-baseline-scenario1   benches/comparisons/ferriskey-baseline
baseline-scenario1             benches/comparisons/baseline
probe-ferriskey                benches/perf-invest/envelope-probe
probe-redis-rs                 benches/perf-invest/envelope-probe
```

Per run sequence:

1. `valkey-cli -p 6379 FLUSHDB`
2. `sleep 2` (calm)
3. Run binary, record headline line
4. Next alternation

Alternation shape per workload: warm-discard(ferriskey) →
warm-discard(redis-rs) → fk1 → rr1 → fk2 → rr2 → fk3 → rr3 → fk4 →
rr4 → fk5 → rr5.  Discard passes dropped; 5 real runs per client per
workload recorded below.

Fixed-5-runs, no median-stabilization — we want the spread to be
visible, not hidden.

## §3 Scenario 1 — RPUSH / BLMPOP / INCR, 10 000 tasks × 16 workers

### ferriskey-baseline

| run | ops/sec | p99 ms | blmpop_outcomes          |
| --: | ------: | -----: | ------------------------ |
|  1  |  8117.2 |  0.36  | ok=10000 nil=15  err=0   |
|  2  |  8167.9 |  0.38  | ok=10000 nil=15  err=0   |
|  3  |  8090.9 |  0.36  | ok=10000 nil=15  err=0   |
|  4  |  8188.5 |  0.35  | ok=10000 nil=15  err=0   |
|  5  |  8127.6 |  0.36  | ok=10000 nil=15  err=0   |

- **median ops/sec: 8127.6**
- min 8090.9, max 8188.5
- spread (max-min)/median = **1.20 %**
- no run drifts > 5 % off median (biggest is run 4 at +0.75 %)

### baseline (redis-rs 1.2.0)

| run | ops/sec | p99 ms | blmpop_outcomes          |
| --: | ------: | -----: | ------------------------ |
|  1  | 14752.0 |  0.37  | ok=10000 nil=0   err=15  |
|  2  | 14771.1 |  0.38  | ok=10000 nil=0   err=15  |
|  3  | 14782.6 |  0.38  | ok=10000 nil=0   err=15  |
|  4  | 14796.3 |  0.38  | ok=10000 nil=0   err=15  |
|  5  | 14799.5 |  0.37  | ok=10000 nil=0   err=15  |

- **median ops/sec: 14782.6**
- min 14752.0, max 14799.5
- spread (max-min)/median = **0.32 %**
- no run drifts > 5 % off median

### Pre-change BLMPOP gap

redis-rs median − ferriskey median = -6655 ops/s → ferriskey is at
**54.98 %** of redis-rs, i.e. a **-45.02 %** deficit on this
workload at these medians. Matches the investigation's earlier
-45.83 % number within run-to-run variance.

## §4 Envelope probe — 1 worker × 100 000 iter INCR + 1 000 warmup (discarded)

### probe-ferriskey (median per percentile across 5 runs)

| run | p50 ms | p95 ms | p99 ms | p99.9 ms | mean ms |
| --: | -----: | -----: | -----: | -------: | ------: |
|  1  | 0.031  | 0.037  | 0.044  | 0.069    | 0.032   |
|  2  | 0.031  | 0.040  | 0.044  | 0.068    | 0.032   |
|  3  | 0.032  | 0.040  | 0.046  | 0.070    | 0.033   |
|  4  | 0.031  | 0.041  | 0.045  | 0.066    | 0.033   |
|  5  | 0.031  | 0.036  | 0.043  | 0.064    | 0.032   |

- median p50 = 0.031 ms  (spread 3.2 %)
- median p95 = 0.040 ms  (spread 12.5 %)
- median p99 = 0.044 ms  (spread 6.8 %)
- median p99.9 = 0.068 ms  (spread 8.8 %)
- median mean = 0.032 ms  (spread 3.1 %)

### probe-redis-rs

| run | p50 ms | p95 ms | p99 ms | p99.9 ms | mean ms |
| --: | -----: | -----: | -----: | -------: | ------: |
|  1  | 0.030  | 0.034  | 0.042  | 0.061    | 0.031   |
|  2  | 0.030  | 0.035  | 0.043  | 0.060    | 0.031   |
|  3  | 0.031  | 0.038  | 0.042  | 0.061    | 0.031   |
|  4  | 0.030  | 0.036  | 0.042  | 0.059    | 0.031   |
|  5  | 0.031  | 0.038  | 0.043  | 0.063    | 0.032   |

- median p50 = 0.030 ms  (spread 3.3 %)
- median p95 = 0.036 ms  (spread 11.1 %)
- median p99 = 0.042 ms  (spread 2.4 %)
- median p99.9 = 0.061 ms  (spread 6.6 %)
- median mean = 0.031 ms  (spread 3.2 %)

### Pre-change single-client INCR gap

ferriskey p50 0.031 ms vs redis-rs p50 0.030 ms — **~3 % at p50**.
This is at the 1-microsecond timer resolution of the probe; a
change smaller than that cannot be distinguished in a single run.
The 5-run median stabilises the picture but cannot cross that
floor.

## §5 Noise-floor summary

| metric                      | spread (pre-fk) | spread (pre-rr) |
| --------------------------- | --------------: | --------------: |
| scenario-1 ops/sec          | 1.20 %          | 0.32 %          |
| scenario-1 p99              | 6.6 %           | 2.9 %           |
| probe p50                   | 3.2 %           | 3.3 %           |
| probe p95                   | 12.5 %          | 11.1 %          |
| probe p99                   | 6.8 %           | 2.4 %           |

- Scenario-1 ferriskey throughput is the tightest metric at
  **±1.2 %**. Any post-change throughput delta ≥ 1.2 % is above
  the observed spread and potentially resolvable; smaller than that
  is inside the pre-run noise band.
- Probe p50 is pinned at the timer floor (0.030/0.031 ms — 3 %
  spread is literally one integer-microsecond of jitter at this
  scale). The probe surfaces envelope cost at p95-p99.9 where there
  is room above the timer.
- Probe p95 spread is > 10 % on both clients → p95 is NOT a useful
  noise-floor anchor. p99.9 (~7 %) and scenario-1 ops/sec (~1.2 %)
  are.

### What this means for Tier-1

W1 predicted ~2 % at p50 on INCR. **The probe p50 is at the 1 µs
timer floor; a 2 % shift on 31 µs is 0.62 µs — below the timer's
integer resolution.** Pre-captured data shows the p50 cannot move
by less than one integer microsecond in a single run, and the 5-run
median only cycles between 0.030 / 0.031 / 0.032. The Tier-1 p50
uplift is likely unresolvable on this probe; the envelope win would
have to surface at p95 / p99 / p99.9 or in scenario-1's throughput
metric.

Scenario-1 throughput has the best noise floor (±1.2 %), but BLMPOP
HOL multiplex-serialization dominates that workload (see
`report-w2-round2.md` §3, `blocking-probe/results/FINDINGS.md`) —
the Tier-1 envelope change may not be the bottleneck there.

Pre-baseline is locked. Post run + delta lands on
`feat/tier1-envelope-collapse` as `tier1-post.md` / `tier1-delta.md`.
