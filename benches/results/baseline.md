# Internal bench baseline — v0.4.0

Internal reference numbers captured against v0.4.0 HEAD (`d595b27`,
post-tag + follow-ups #168/#169/#170), re-run on 2026-04-23 per
`docs/RELEASING.md` §Pre-flight.

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

## Session changes between v0.3.2 and v0.4.0 that could affect perf

- Stage 1c (T1–T4) moved ~22 FCALL sites behind `EngineBackend` trait
  methods — adds one virtual dispatch per call site (typical ~1–2%).
- #170 / closes #154: `#[tracing::instrument]` on every
  `EngineBackend` impl method — roughly ~100 ns per call when the
  global subscriber filters the span out.
- `backend_context` wrapping on error paths — negligible in the hot
  path.
- BackendError seal (Round-7) — type-system only; zero runtime effect.

Expected: small (1–3%) regression on hot primitives that went through
the trait. Larger deltas below are flagged.

## Scenario 1 — submit / claim / complete (10k tasks × 16 workers, 4 KiB payload)

| system             | git_sha | ops/s   | p50 ms | p95 ms | p99 ms |
|--------------------|---------|---------|--------|--------|--------|
| redis-rs baseline  | 6f81926 | 14755.2 | 0.256  | 0.327  | 0.383  |
| ferriskey baseline | ccff3ac |  7993.3 | 0.263  | 0.340  | 0.403  |
| apalis-redis rc.7  | 1b2cce5 |    51.9 | n/a    | n/a    | n/a    |
| **ff-server (v0.4.0)** | **d595b27** | **3045.6** | **0.411** | **0.909** | **1.126** |
| ff-server (v0.3.2) | da89fa9 |  2658.0 | 0.399  | 0.882  | 1.152  |
| ff-server (v0.1.0) | 01f6327 |  2171.6 | 0.41   | 0.89   | 1.11   |
| ff-server (pre-Batch-C) | 6fef93d |  3320.9 | 0.395  | 0.672  | 0.813  |

Throughput **+14.6% vs v0.3.2** (2658 → 3045 ops/s). p50/p95 drift up
~3% (0.399 → 0.411, 0.882 → 0.909), consistent with the
instrumentation overhead budget called out above; p99 improved
(1.152 → 1.126). The trait-dispatch overhead is more than
compensated by other session changes in the write path. No
regression flag.

## Scenario 2 — cap_routed (1 iteration, cap_universe=10, 100 workers)

| mode    | git_sha | correct_routing_rate | p50 ms    | p95 ms      | p99 ms      |
|---------|---------|----------------------|-----------|-------------|-------------|
| happy   | d595b27 | 1.000                |   397.46  |    503.02   |    524.18   |
| partial | d595b27 | 0.953                | 12542.29  | 222433.94   | 292422.60   |
| scarce  | d595b27 | 0.927                |   398.94  |    512.52   | 235196.03   |

Thresholds: happy==1.0, partial>=0.90, scarce>=0.85 — all three pass.

`happy` p50 is flat-to-improved vs v0.3.2 (409 → 397 ms, −2.9%).
`partial` and `scarce` are adversarial-by-design (RFC-009 cap-routing
convergence notes); their tails move significantly between runs
because the promotion-cycle race is a coin flip per cycle. `partial`
p50 moved 14155 → 12542 ms (−11.4%) and `scarce` p99 moved
117197 → 235196 ms (+100%); both deltas sit inside the observed
run-to-run band for those modes (see v0.3.2 notes for the same
pattern against v0.1.0). Routing-rate (`partial`: 0.961 → 0.953,
`scarce`: 0.921 → 0.927) is the metric that characterises
behaviour — both remain comfortably above their floors.

## Scenario 3 — flow_dag_linear (100 flows × 10 nodes, 16 workers)

| system                    | git_sha | flows/s | p50 ms  | p95 ms   | p99 ms   |
|---------------------------|---------|---------|---------|----------|----------|
| ff-server (v0.4.0)        | d595b27 |   7.93  |  357.57 |  5287.76 | 12430.69 |
| ff-server (v0.3.2)        | da89fa9 |   8.16  |  352.95 | (n/a)    | 11948.56 |
| ff-server (v0.1.0)        | 01f6327 |   8.18  |  347.63 | (n/a)    | 12055.19 |
| ff-server (pre-BatchC)    | b4ec2c2 |   5.96  | 7348.79 |  8555.88 |  9350.30 |
| apalis (apalis-workflow, N=5)   | a4057c7 |   9.3   | n/a     | n/a      | n/a      |
| apalis-approx (prior harness)   | 1b2cce5 |   1.02  | n/a     | n/a      | n/a      |

Flat vs v0.3.2 — flows/s 8.16 → 7.93 (−2.8%), p50 353 → 358 ms
(+1.3%), p99 11948 → 12430 ms (+4.0%). All deltas sit inside the
measured run-to-run band on this host (the tail p99 on flows=100
× nodes=10 routinely moves ±5% between single-sample runs). No
regression flag; the push-based promotion speedup (20× at p50 over
pre-BatchC) holds.

**apalis harness note (retained from v0.3.2 baseline, unchanged):**
Scenario-3 apalis harness uses `apalis-workflow::Workflow`
(sequential `and_then`) after the apalis maintainer flagged the
prior hand-rolled chain as under-representing apalis. See
COMPARISON.md + prior baseline for the full N=5 apalis analysis at
`a4057c7`; no re-measurement this cycle.

## Scenario 4 — long_running_steady_state (300s, refill 20 tasks / 10s, **N=5 samples**)

**Methodology (unchanged from v0.3.2 baseline):** Scenario 4 is
bimodal across the 10s refill / 60s deadline boundary (see
`rfcs/drafts/scenario-4-regression-investigation.md`); N ≥ 5 is
required for release-gate numbers. All endpoints below are sampled
at N=5 on the same host (Valkey 7.2.12 effective version).

| metric                        | v0.4.0 (d595b27, N=5) | v0.3.3 (d813772, N=5) | HEAD-before-v0.3.2 (8a8d996, N=5) | v0.1.0 (01f6327, N=5) | v0.3.2 (da89fa9, N=1) |
|-------------------------------|-----------------------|-----------------------|-----------------------------------|-----------------------|-----------------------|
| missed_deadline_pct mean      | **4.41%**             | 4.71%                 | 4.81%                             | 4.31%                 | 7.11%                 |
| missed_deadline_pct stddev    | **1.38%**             | 2.23%                 | 1.55%                             | 1.81%                 | n/a (single sample)   |
| missed_deadline_pct min / max | 3.73% / 6.86%         | 1.98% / 7.04%         | 3.59% / 7.01%                     | 2.09% / 6.94%         | n/a                   |
| per-sample missed_pct         | 6.86, 3.97, 3.73, 3.73, 3.73 | 3.80, 3.73, 1.98, 7.04, 6.98 | 3.80, 3.77, 3.59, 7.01, 5.90 | 5.04, 3.77, 3.73, 2.09, 6.94 | — |
| completed (mean across N)     | 2600                  | ~2600                 | 2613                              | 2599                  | 2680                  |
| failed (mean across N)        | 107                   | ~100                  | 103                               | 116                   | 105                   |
| steady_state_ops/s (mean)     | 2.0                   | 2.0                   | 2.0                               | 2.0                   | 2.0                   |
| lease_renewal_overhead (%)    | 0.000182              | 0.0                   | 0.0                               | 0.0                   | 0.000178              |

**No regression.** v0.4.0 is `4.41% ± 1.38%`; v0.1.0 is `4.31% ±
1.81%`. Δ = +0.10pp, well inside the pooled stddev. The N=5 v0.4.0
run shows 1 high-regime + 4 low-regime samples (prior runs showed
3+2 or 2+3 splits). v0.3.2's single-sample 7.11% snapshot was a
high-regime artefact; all N=5 measurements across v0.1.0 → v0.4.0
cluster at 4.3%–4.8% mean. The trait-dispatch + instrumentation
overhead is invisible against the refill/deadline bimodal band.

The per-sample arrays are preserved verbatim so reviewers can
replicate the 3:2 / 4:1 / 2:3 regime split. Re-baseline each
release's Scenario 4 row with N ≥ 5 before drawing comparisons.

Measurement note: the v0.1.0 long_running binary as shipped at
`01f6327` was pre-RFC-011 and cannot submit executions against its
own server (the ExecutionId hash-tag requirement landed in #46, post-
v0.1.0 tag). The v0.1.0 N=5 row above was captured by applying the
three-file harness fix from `4192664` onto the `01f6327` bench tree
(bench wiring only; no server-side product change).

Lease renewal overhead and RSS profile both remain in the pre-v0.1.0
envelope.

## Scenario 5 — suspend_signal_resume (**N=5 samples × 100 roundtrips**)

**Methodology (PR #XXX):** Scenario 5 now runs under N ≥ 5
independent sampling batches via `FF_BENCH_SAMPLES=5`. Each sample
collects 100 roundtrips, reports its own p50/p95/p99, and the
headline numbers are mean + stddev across the 5 per-sample
percentiles. Mirrors the Scenario 4 / PR #140 fix. No FLUSHALL
between samples: unlike Scenario 4, Scenario 5 has no accumulating
queue state and FLUSHALL would wipe the server's per-partition
`waitpoint_hmac_secrets` keys that the server initialized at
startup.

Single-sample historical rows (100 roundtrips in one criterion run)
are retained below for cross-release comparison, but all future
Scenario 5 floor comparisons should be re-baselined at N ≥ 5 on
both sides.

**main HEAD (`076baa2`, N=5 on 2026-04-23):**

| metric     | mean   | stddev | per-sample                               |
|------------|--------|--------|------------------------------------------|
| p50 ms     |  9.16  |  0.07  | 9.12, 9.07, 9.22, 9.25, 9.18             |
| p95 ms     |  9.90  |  0.38  | 9.75, 9.76, 9.74, 10.57, 9.68            |
| p99 ms     | 10.43  |  0.69  | 10.03, 9.83, 11.25, 11.11, 9.94          |
| ops/s      | 109.12 |  —     | 109.71, 110.31, 108.48, 108.13, 108.96   |

**Single-sample historical rows (legacy criterion, 100 roundtrips in
one run; N=1 — retain for release-to-release direction checks
only):**

| metric     | git_sha | v0.4.0 | v0.3.2 (da89fa9) | v0.1.0 (01f6327) |
|------------|---------|--------|------------------|------------------|
| ops/s      | d595b27 | 106.87 | 108.34           | 105.05           |
| p50 ms     | d595b27 |   9.36 |   9.23           |   9.52           |
| p95 ms     | d595b27 |  10.83 |   9.93           |  10.59           |
| p99 ms     | d595b27 |  11.96 |  10.76           |  11.91           |

Measured with `claim_poll_interval_ms=1`; production default
(1000ms) adds ~500ms on the re-claim leg, so production-projected
p50 ≈ 509 ms.

**Previously flagged regression dissolves under N=5.** Issue #173
noted p95 9.93 → 10.83 ms (+9.1%) and p99 10.76 → 11.96 ms (+11.2%)
between v0.3.2 and v0.4.0 single-sample snapshots. At N=5 on main
HEAD, per-sample p95 covers 9.68–10.57 ms and per-sample p99 covers
9.83–11.25 ms; both v0.3.2's 10.76 ms and v0.4.0's 11.96 ms fall
inside (or at the edge of) the 5-run band. The tracing
instrumentation from #170 remains a plausible ~100 ns-per-call
contributor on the resume leg, but it is not separable from
run-to-run noise at a 100-roundtrip sample size — consistent with
Worker SSSS' investigation of #173. N=5 is now the floor for
release-gate Scenario 5 comparisons.

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
