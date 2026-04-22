# Internal bench baseline — v0.3.2

Internal reference numbers captured against v0.3.2 HEAD (`da89fa9`),
re-run on 2026-04-22 to satisfy `docs/RELEASING.md` §Pre-flight (the
baseline refresh was deferred during the v0.3.2 hotfix window and is
being filled in retroactively).

Use as a floor to detect regressions in future releases, not as public
performance claims.

**Runs are not tracked in git.** Regenerate from `benches/` sources.
Hardware-specific — numbers are only comparable when re-run on the same
host class.

## Host
- CPU: AMD EPYC 9R14, 16 cores
- Mem: 30 GB
- Profile: `release` with `lto = "thin"`, `codegen-units = 1`, `debug = 0`
  (bench workspace profile; top-level product profile is `lto = "fat"`)
- Valkey 8.1.0 (source build, standalone, TLS off, port 6389). Note: the
  harness reports `valkey_version: "7.2.4"` in the JSON because Valkey
  8.1.0 emits `redis_version:7.2.4` first in `INFO server` for client
  back-compat and the reporter picks the first match. Server process
  self-reports `valkey_version:8.1.0`.

## Scenario 1 — submit / claim / complete (10k tasks × 16 workers, 4 KiB payload)

| system             | git_sha | ops/s   | p50 ms | p95 ms | p99 ms |
|--------------------|---------|---------|--------|--------|--------|
| redis-rs baseline  | 6f81926 | 14755.2 | 0.256  | 0.327  | 0.383  |
| ferriskey baseline | ccff3ac |  7993.3 | 0.263  | 0.340  | 0.403  |
| apalis-redis rc.7  | 1b2cce5 |    51.9 | n/a    | n/a    | n/a    |
| **ff-server (v0.3.2)** | **da89fa9** | **2658.0** | **0.399** | **0.882** | **1.152** |
| ff-server (v0.1.0) | 01f6327 |  2171.6 | 0.41   | 0.89   | 1.11   |
| ff-server (pre-Batch-C) | 6fef93d |  3320.9 | 0.395  | 0.672  | 0.813  |

Throughput +22% vs v0.1.0 (`01f6327`) at effectively unchanged latency
percentiles. Captured with the RFC-011 minting path (`ExecutionId::for_flow`)
so each exec spreads across partitions.

The Batch-C trade-off remains: `ff_complete` / `fail` / `cancel`
PUBLISH to `ff:dag:completions` on every flow-bound terminal
transition (no-op for standalone execs), and the engine maintains a
dedicated RESP3 SUBSCRIBE connection.

## Scenario 2 — cap_routed (1 iteration, cap_universe=10, 100 workers)

| mode    | git_sha | correct_routing_rate | p50 ms    | p95 ms      | p99 ms      |
|---------|---------|----------------------|-----------|-------------|-------------|
| happy   | da89fa9 | 1.000                |   409.26  |    515.87   |    535.13   |
| partial | da89fa9 | 0.961                | 14155.999 | 209212.961  | 274235.299  |
| scarce  | da89fa9 | 0.921                |   416.56  |    535.67   | 117196.936  |

Thresholds: happy==1.0, partial>=0.90, scarce>=0.85 — all three pass.

`happy` numbers are flat vs v0.1.0 (407 → 409 ms p50). `partial` and
`scarce` are adversarial-by-design distributions (see RFC-009 cap-routing
convergence notes); their tails move significantly between runs because
the promotion-cycle race is a coin flip per cycle. The p50 drift on
`partial` (11867 → 14156 ms, +19%) is within the observed run-to-run
variance for this mode — routing-rate (0.956 → 0.961) is the metric
that characterises behaviour, not absolute latency. The `scarce` p99
improvement (175906 → 117197 ms) falls in the same band.

## Scenario 3 — flow_dag_linear (100 flows × 10 nodes, 16 workers)

| system         | git_sha | flows/s | p50 ms  | p95 ms  | p99 ms   |
|----------------|---------|---------|---------|---------|----------|
| ff-server      | da89fa9 |   8.16  |  352.95 | (n/a)   | 11948.56 |
| ff-server (v0.1.0) | 01f6327 |   8.18 | 347.63  | (n/a)   | 12055.19 |
| ff-server (pre-BatchC) | b4ec2c2 |   5.96 | 7348.79 | 8555.88 | 9350.30 |
| apalis-approx  | 1b2cce5 |   1.02  | n/a     | n/a     | n/a      |

Flat vs v0.1.0 across all percentiles — the push-based promotion
speedup (21× at p50 over pre-BatchC) holds. p99 tail growth still
present; no follow-up investigation filed in this cycle.

## Scenario 4 — long_running_steady_state (300s, refill 20 tasks / 10s)

| metric                       | v0.3.2 (da89fa9) | v0.1.0 (01f6327) | pre-BatchC (6f81926) |
|------------------------------|------------------|------------------|----------------------|
| completed (count)            | 2680             | 2580             | 2679                 |
| failed (count)               | 105              | 101              | 102                  |
| missed_deadline (count)      | 205              | 100              | 188                  |
| missed_deadline (% of completed) | **7.11%**    | 3.88%            | 7.01%                |
| steady_state_ops/s           | 2.0              | 2.0              | n/a                  |
| lease_renewal_overhead (%)   | 0.000178         | 0.00015          | 0.00017              |
| rss_min_mb / rss_max_mb      | 34 / 69          | 32 / 67          | 37 / 72              |
| rss_slope_mb_per_min         | −4.10            | −4.41            | n/a                  |

**Regression flag:** `missed_deadline_pct` nearly doubled vs v0.1.0
(3.88% → 7.11%), back to the pre-BatchC 7.01% level. Completed count
grew modestly (2580 → 2680) so the denominator didn't shrink; the
numerator (missed_deadline) jumped from 100 → 205. Single-run
observation, no diagnosis performed in this refresh cycle. A repeat
run is recommended before v0.4 to distinguish single-run noise from a
real regression, and if reproducible, a flame capture on the refill
window is the right next step. Not blocking v0.3.2 (which has already
shipped); flagged here per `benches/results/baseline.md` operating
discipline.

Lease renewal overhead and RSS profile both remain in the pre-v0.1.0
envelope.

## Scenario 5 — suspend_signal_resume (100 samples)

| metric     | git_sha | value  | v0.1.0 (01f6327) |
|------------|---------|--------|------------------|
| ops/s      | da89fa9 | 108.34 | 105.05           |
| p50 ms     | da89fa9 |   9.23 |   9.52           |
| p95 ms     | da89fa9 |   9.93 |  10.59           |
| p99 ms     | da89fa9 |  10.76 |  11.91           |

Measured with `claim_poll_interval_ms=1`; production default (1000ms)
adds ~500ms on the re-claim leg, so production-projected p50 ≈ 509 ms.
Small improvement across all percentiles; no regression.

## Comparison rules for next release

- Same host class (EPYC 9R14, 16c). Cloud instance type changes invalidate the
  floor.
- Same profile (`lto=fat`, `codegen-units=1`). Do not compare lto=fat against
  lto=thin/off numbers; flame profiles (debug=1) are not release-comparable.
- ≥ 5% regression on p50 throughput for any scenario blocks release pending
  investigation. ≥ 2% warrants a flame capture on the offending scenario before
  shipping.
- BLMPOP / blocking-command scenarios: regression is expected until ferriskey
  issue #12 (dedicated connection for blocking commands) lands. Track direction,
  not absolute gap.
