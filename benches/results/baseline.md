# Internal bench baseline — v0.1.0

Internal reference numbers from the last run before `benches/results/` was moved
out of version control. Use as a floor to detect regressions in future releases,
not as public performance claims.

**Runs are not tracked in git.** Regenerate from `benches/` sources. Hardware-
specific — numbers are only comparable when re-run on the same host class.

## Host
- CPU: AMD EPYC 9R14, 16 cores
- Mem: 30 GB
- Profile: `release` with `lto = "fat"`, `codegen-units = 1`, `debug = 0`

## Scenario 1 — submit / claim / complete (10k tasks × 16 workers, 4 KiB payload)

| system             | git_sha | ops/s   | p50 ms | p95 ms | p99 ms |
|--------------------|---------|---------|--------|--------|--------|
| redis-rs baseline  | 6f81926 | 14755.2 | 0.256  | 0.327  | 0.383  |
| ferriskey baseline | ccff3ac |  7993.3 | 0.263  | 0.340  | 0.403  |
| apalis-redis rc.7  | 1b2cce5 |    51.9 | n/a    | n/a    | n/a    |
| ff-server full     | 6fef93d |  3320.9 | 0.395  | 0.672  | 0.813  |
| ff-server (older)  | fb5754d |  3008.8 | 0.384  | 0.671  | 0.786  |

Gap at round-1 capture: ferriskey vs redis-rs ≈ −45.8% ops/s. Round-3 cross-check
(post-telemetry + lazy redesign) compressed to ≈ −41.8%. Mechanism traced to mux
HOL serialization on BLMPOP, tracked upstream as ferriskey issue #12.

## Scenario 2 — cap_routed (1 iteration, cap_universe=10)

| mode    | git_sha | correct_routing_rate | p50 ms    | p95 ms      | p99 ms      |
|---------|---------|----------------------|-----------|-------------|-------------|
| happy   | 6f81926 | 1.000                |   432.10  |    546.16   |    571.37   |
| partial | 6f81926 | 0.937                | 13434.59  | 213436.96   | 283366.34   |
| scarce  | 6f81926 | 0.925                |   416.90  |    536.89   | 203063.68   |

## Scenario 3 — flow_dag_linear (100 flows × 10 nodes, 16 workers)

| system         | git_sha | flows/s | p50 ms  | p95 ms  | p99 ms  |
|----------------|---------|---------|---------|---------|---------|
| ff-server      | b4ec2c2 |   5.959 | 7348.79 | 8555.88 | 9350.30 |
| apalis-approx  | 1b2cce5 |   1.022 | n/a     | n/a     | n/a     |

apalis has no DAG primitive; apalis-approx is a hand-wired 10-stage chain (no
data passing, no flow-level retry/cancel). Not apples-to-apples for features,
but brackets order-of-magnitude.

## Scenario 4 — long_running_steady_state (300s, refill 20 tasks / 10s)

| metric                      | value     |
|-----------------------------|-----------|
| git_sha                     | 6f81926   |
| completed                   | 2679      |
| failed                      | 102       |
| missed_deadline (%)         | 7.01      |
| lease_renewal_overhead (%)  | 0.00017   |
| rss_min_mb / rss_max_mb     | 37 / 72   |
| rss_growth_pct (end vs max) | −44.78    |

## Scenario 5 — suspend_signal_resume (7 samples × 10s)

| metric     | git_sha | value           |
|------------|---------|-----------------|
| ops/s      | 6f81926 |  99.86          |
| p50 ms     | 6f81926 |  10.01          |
| p99 ms     | 6f81926 |  10.39          |

Measured with `claim_poll_interval_ms=1`; production default (1000ms) adds ~500ms
on the re-claim leg, so production-projected p50 ≈ 510 ms.

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
