# FlowFabric Benchmarks

Workspace-isolated benchmark harness for FlowFabric, plus side-by-side
comparison implementations against hand-rolled Valkey (ground truth) and
(in follow-up work) apalis + faktory-rs.

This directory sits OUTSIDE the main FlowFabric workspace — the
top-level `Cargo.toml` excludes `benches/` so bench dependencies
(criterion, later apalis + faktory + redis-rs) never leak into the
product crates. Run bench commands from within this directory.

## Layout

```
benches/
  Cargo.toml                     # local workspace root
  harness/                       # Phase A: scenario 1
    benches/                     # criterion integrations
    src/                         # shared report + workload helpers
  comparisons/
    baseline/                    # Phase A: redis-rs + BLMPOP ground truth
    apalis/                      # Phase C (follow-up)
    faktory/                     # Phase C (follow-up)
  results/                       # JSON reports + tagged baselines
    COMPARISON.md                # side-by-side tables
  check-release.sh               # release gate entry point
  scripts/check_release.py       # threshold logic
```

## Running scenario 1 (Phase A)

Prerequisites:

1. Valkey on `localhost:6379`:
   ```
   valkey-cli -p 6379 FLUSHALL   # only if you want a clean slate
   ```
2. `ff-server` with HMAC secret and the `bench` lane:
   ```
   FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
   FF_LANES=default,bench \
     cargo run -p ff-server --release
   ```

Then from `benches/`:

```
cargo bench --bench submit_claim_complete
```

JSON output lands at `benches/results/submit_claim_complete-<sha>.json`
alongside criterion's HTML in `target/criterion/`.

Override knobs (all optional):

| env                       | default                | notes |
| ------------------------- | ---------------------- | ----- |
| `FF_BENCH_SERVER`         | `http://localhost:9090`|       |
| `FF_BENCH_VALKEY_HOST`    | `localhost`            |       |
| `FF_BENCH_VALKEY_PORT`    | `6379`                 |       |
| `FF_BENCH_NAMESPACE`      | `default`              |       |
| `FF_BENCH_LANE`           | `bench`                | server must be started with this lane in `FF_LANES` |
| `FF_BENCH_CLUSTER`        | `false`                | set to `true` + cluster endpoint to run against cluster |
| `FF_BENCH_LARGE`          | unset                  | `true` adds a 100_000-task size to criterion (slow) |
| `FF_API_TOKEN`            | unset                  | forwarded as `Authorization: Bearer` |

## Running the baseline (Phase A)

The `baseline` package uses raw `redis-rs` — no FlowFabric code, no
execution engine. "How fast does Valkey itself go?" is the reference
point for interpreting every other number.

```
cd benches/comparisons/baseline
cargo run --release --bin baseline-scenario1 -- --tasks 10000
```

Output: `benches/results/submit_claim_complete-baseline-<sha>.json`.

## Release gate

`check-release.sh` is the gate W1's `release.yml` invokes before
tagging. It:

1. Confirms `ff-server` is reachable, else exits 2 (infrastructure
   failure, not a regression).
2. Runs every Phase A scenario end-to-end.
3. If a prior `v*.*.*` tag exists AND `benches/results/<tag>/` is
   populated, compares current runs against it using
   `scripts/check_release.py`.
4. First-run-ever path: snapshots current numbers as the baseline and
   exits 0 so we don't have a chicken-and-egg dependency blocking the
   first release.

Thresholds:

| severity | throughput regression | p99 regression |
| -------- | --------------------- | -------------- |
| FAIL     | > 10 %                | > 20 %         |
| WARN     | > 5 %                 | > 10 %         |

Start lenient — if real regressions prove noisier than this, tighten
in a follow-up. Starting too strict trains people to ignore red CI.

## Follow-up work

Phase A ships only scenario 1 + the baseline. Remaining scenarios are
tracked in Batch C issues:

| issue item | priority | description                                          |
| ---------- | -------- | ---------------------------------------------------- |
| 6.1        | high     | scenario 2 — suspend/signal/resume (HMAC roundtrip)  |
| 6.2        | high     | scenario 5 — capability-routed claim (differentiator)|
| 6.3        | medium   | scenario 4 — 10-node linear flow DAG                 |
| 6.4        | medium   | scenario 3 — 5-min steady-state 100-worker load      |
| 7.1        | medium   | apalis comparison (scenarios 1, 2, 4)                |
| 7.2        | low      | faktory comparison (scenarios 1, 3)                  |

Each Phase B/C scenario should write the same JSON schema and plug
into the `SCENARIOS=(...)` list in `check-release.sh`.

## Schema

Every report matches this shape (see `harness/src/report.rs` for the
canonical source):

```json
{
  "scenario": "submit_claim_complete",
  "system": "flowfabric",
  "git_sha": "17a4034",
  "valkey_version": "8.0.1",
  "host": { "cpu": "…", "cores": 16, "mem_gb": 30 },
  "cluster": false,
  "timestamp_utc": "2026-04-17T09:00:00Z",
  "config": { "tasks": 10000, "workers": 16, "payload_bytes": 4096 },
  "throughput_ops_per_sec": 1234.5,
  "latency_ms": { "p50": 4.2, "p95": 18.1, "p99": 47.3 }
}
```
