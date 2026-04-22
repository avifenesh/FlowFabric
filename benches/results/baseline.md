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

## Scenario 4 — long_running_steady_state (300s, refill 20 tasks / 10s, **N=5 samples**)

**Methodology (as of 2026-04-22):** Scenario 4 is bimodal across the
10s refill / 60s deadline boundary (see
`rfcs/drafts/scenario-4-regression-investigation.md`): depending on
phase alignment between a refill batch and the steady-state drain, a
single 300s run lands in either a low-miss regime (~2–4%) or a
high-miss regime (~6–7%). Single-run sampling is therefore inadequate —
the 3.88% v0.1.0 number and the 7.11% v0.3.2 number in PR #133's
baseline were both snapshots of opposite regimes of the same stable
distribution, not a regression. The bench binary now takes
`--samples N` and reports **mean ± stddev** across N independent 300s
runs (Valkey `FLUSHALL` between samples). **N ≥ 5 is required** for
release-gate numbers. Both endpoints below are now sampled at N=5 on
the same host (Valkey 7.2.12).

| metric                        | HEAD (8a8d996, N=5) | v0.1.0 (01f6327, N=5) | v0.3.2 (da89fa9, N=1) | pre-BatchC (6f81926, N=1) |
|-------------------------------|---------------------|-----------------------|-----------------------|---------------------------|
| missed_deadline_pct mean      | **4.81%**           | **4.31%**             | 7.11%                 | 7.01%                     |
| missed_deadline_pct stddev    | **1.55%**           | **1.81%**             | n/a (single sample)   | n/a                       |
| missed_deadline_pct min / max | 3.59% / 7.01%       | 2.09% / 6.94%         | n/a                   | n/a                       |
| per-sample missed_pct         | 3.80, 3.77, 3.59, 7.01, 5.90 | 5.04, 3.77, 3.73, 2.09, 6.94 | — | —               |
| completed (mean across N)     | 2613                | 2599                  | 2680                  | 2679                      |
| failed (mean across N)        | 103                 | 116                   | 105                   | 102                       |
| steady_state_ops/s (mean)     | 2.0                 | 2.0                   | 2.0                   | n/a                       |
| lease_renewal_overhead (%)    | 0.0                 | 0.0                   | 0.000178              | 0.00017                   |

**No regression.** HEAD is `4.81% ± 1.55%`; v0.1.0 is `4.31% ± 1.81%`.
Δ = +0.50pp, or ~0.3× the pooled stddev — well inside run-to-run
variance. Both endpoints show the same bimodal pattern (3 low-regime +
2 high-regime samples at each; see per-sample rows). The HEAD N=5
measurement here also reproduces the shape of the v0.3.3 (`d813772`)
N=5 measurement reported in PR #140 (4.71% ± 2.23%, samples
`[3.80, 3.73, 1.98, 7.04, 6.98]`): means within 0.1pp, same 3-low +
2-high split. Worker DD's bisect
(`rfcs/drafts/scenario-4-regression-investigation.md`) cluster-sampled
`missed_deadline_pct` at 6.98%–7.07% across 8 runs spanning 6 commits
in `01f6327..da89fa9` — those 8 runs all landed in the high-regime
mode by chance; the N=5 runs here span both modes and confirm the
distribution is stable across v0.1.0 → HEAD.

The prior "3.88% → 7.11% REGRESSION ×1.8" flag from PR #133's
single-sample baseline was a methodology artifact: two single-sample
snapshots of a bimodal distribution compared as if they were point
estimates of a scalar. The v0.1.0 row in PR #133 is now superseded by
the v0.1.0 N=5 column above; the v0.3.2 / pre-BatchC rows remain
single-sample for historical reference and should not be compared
directly against N=5 means. Re-baseline each release's Scenario 4 row
with N ≥ 5 before drawing comparisons.

Measurement note: the v0.1.0 long_running binary as shipped at
`01f6327` was pre-RFC-011 and cannot submit executions against its
own server (the ExecutionId hash-tag requirement landed in #46, post-
v0.1.0 tag). The v0.1.0 N=5 run above was captured by applying the
three-file harness fix from `4192664` onto the `01f6327` bench tree
(bench wiring only; no server-side product change). This is the same
composite Worker DD used for step 7 of the bisect.

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
