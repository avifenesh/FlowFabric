# Internal bench baseline — v0.1.0

Internal reference numbers from the post-Batch-C run on commit `01f6327`.
Use as a floor to detect regressions in future releases, not as public
performance claims.

**Runs are not tracked in git.** Regenerate from `benches/` sources.
Hardware-specific — numbers are only comparable when re-run on the same
host class.

## Host
- CPU: AMD EPYC 9R14, 16 cores
- Mem: 30 GB
- Profile: `release` with `lto = "fat"`, `codegen-units = 1`, `debug = 0`
- Valkey 8.1.0 (source build, standalone, TLS off)

## Scenario 1 — submit / claim / complete (10k tasks × 16 workers, 4 KiB payload)

| system             | git_sha | ops/s   | p50 ms | p95 ms | p99 ms |
|--------------------|---------|---------|--------|--------|--------|
| redis-rs baseline  | 6f81926 | 14755.2 | 0.256  | 0.327  | 0.383  |
| ferriskey baseline | ccff3ac |  7993.3 | 0.263  | 0.340  | 0.403  |
| apalis-redis rc.7  | 1b2cce5 |    51.9 | n/a    | n/a    | n/a    |
| **ff-server (v0.1.0)** | **01f6327** | **2171.6** | **0.41** | **0.89** | **1.11** |
| ff-server (pre-Batch-C) | 6fef93d |  3320.9 | 0.395  | 0.672  | 0.813  |

Note: the 35% headline drop vs `6fef93d` is partly from the RFC-011
single-partition hot-spot that solo-minted execs hit on a single lane
(bench harness fixed to use `ExecutionId::for_flow` post this run).
Latency p99 up ~37% — traceable to the push-based DAG listener's
SUBSCRIBE connection always being up; dispatcher spawning reads one
extra client connection per completion when `flow_id` is empty it
short-circuits, but the SUBSCRIBE itself is steady-state overhead.

## Scenario 2 — cap_routed (1 iteration, cap_universe=10, 100 workers)

| mode    | git_sha | correct_routing_rate | p50 ms    | p95 ms      | p99 ms      |
|---------|---------|----------------------|-----------|-------------|-------------|
| happy   | 01f6327 | 1.000                |   407.09  |    523.71   |    541.93   |
| partial | 01f6327 | 0.956                | 11867.52  | 236849.39   | 281806.90   |
| scarce  | 01f6327 | 0.917                |   396.62  |    505.07   | 175906.66   |

Thresholds: happy==1.0, partial>=0.90, scarce>=0.85 — all three pass.

## Scenario 3 — flow_dag_linear (100 flows × 10 nodes, 16 workers)

| system         | git_sha | flows/s | p50 ms  | p95 ms  | p99 ms   |
|----------------|---------|---------|---------|---------|----------|
| ff-server      | 01f6327 |   8.18  |  347.63 | (n/a)   | 12055.19 |
| ff-server (pre-BatchC) | b4ec2c2 |   5.96 | 7348.79 | 8555.88 | 9350.30 |
| apalis-approx  | 1b2cce5 |   1.02  | n/a     | n/a     | n/a      |

**DAG latency improvement 21×** at p50 (7348ms → 347ms) from Batch C item 6
push-based promotion replacing the `dependency_reconciler` scan interval
as the per-stage floor. p99 shows tail growth — likely worker-pool
contention on refill; investigation follow-up.

apalis has no DAG primitive; apalis-approx is a hand-wired 10-stage chain
(no data passing, no flow-level retry/cancel). Not apples-to-apples for
features, but brackets order-of-magnitude.

## Scenario 4 — long_running_steady_state (300s, refill 20 tasks / 10s)

| metric                      | v0.1.0 (01f6327) | pre-BatchC (6f81926) |
|-----------------------------|------------------|----------------------|
| completed                   | 2580             | 2679                 |
| failed                      | 101              | 102                  |
| missed_deadline             | 100              | n/a (%)              |
| steady_state_ops/s          | 2.0              | n/a                  |
| lease_renewal_overhead (%)  | 0.00015          | 0.00017              |
| rss_min_mb / rss_max_mb     | 32 / 67          | 37 / 72              |
| rss_slope_mb_per_min        | −4.41            | n/a                  |

Completion count roughly stable; lease renewal overhead unchanged.

## Scenario 5 — suspend_signal_resume (100 samples)

| metric     | git_sha | value  |
|------------|---------|--------|
| ops/s      | 01f6327 | 105.05 |
| p50 ms     | 01f6327 |   9.52 |
| p95 ms     | 01f6327 |  10.59 |
| p99 ms     | 01f6327 |  11.91 |

Measured with `claim_poll_interval_ms=1`; production default (1000ms)
adds ~500ms on the re-claim leg, so production-projected p50 ≈ 510 ms.
+5% over pre-Batch-C — the terminal-op `PUBLISH` from Batch C item 6
adds a single Valkey round-trip per suspend / signal / resume cycle.

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
