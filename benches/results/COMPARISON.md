# FlowFabric vs. peer task queues — benchmark comparison

Side-by-side numbers across the five scenarios defined in
`benches/README.md`. Each cell is `throughput_ops_per_sec` on the first
line and `p50 / p95 / p99` latency in milliseconds on the second.

Current HEAD: **`4aac1c8`** (workspace v0.5.0 bump). Host: AMD EPYC 9R14
× 16, Valkey 7.2.4. See `benches/results/*-4aac1c8.json` for full
configs and notes.

## Scenario 1 — submit → claim → complete throughput (10 000 tasks, 16 workers)

| system      | throughput (ops/s) | p50 / p95 / p99 (ms) |
| ----------- | ------------------ | -------------------- |
| flowfabric  | **2 966**          | 0.393 / 0.695 / 0.837 |
| baseline    | **14 462**         | 0.275 / 0.357 / 0.482 |
| apalis      | 98                 | — (not captured)      |
| faktory     | _Phase C_          | _Phase C_             |

*flowfabric throughput is the criterion 10 000-task estimate mean
(3.37 s per 10 k drain). baseline = raw `redis-rs` RPUSH/BLMPOP/INCR
with no leases or flows — it's the Valkey ceiling, not a peer engine.
apalis 1.0.0-rc.7 + `apalis-redis`: default JSON codec, no lease-backed
fencing; `latency_ms` is not surfaced by the apalis harness.*

## Scenario 2 — suspend → signal → resume latency (tight-loop poll)

| system          | p50 (ms)           | p95 (ms)           | p99 (ms)           | throughput (ops/s) |
| --------------- | ------------------ | ------------------ | ------------------ | ------------------ |
| flowfabric      | **9.111**          | **9.988**          | **11.162**         | **109.8**          |
| apalis 🚫       | _no native suspend_| —                  | —                  | —                  |
| faktory 🚫      | _no native suspend_| —                  | —                  | —                  |
| baseline 🚫     | _no primitive_     | —                  | —                  | —                  |

Production-projected p50 with default `claim_poll_interval_ms=1000` is
~509 ms (tight-loop bench uses 1 ms poll). Apalis/faktory/baseline have
no equivalent primitive — retry-middleware workaround measures a
different shape (polled retries + unauthenticated signal keys), not
suspend/resume.

## Scenario 3 — 5-minute steady-state, 100 workers, 10 000-task seed

| metric                                   | flowfabric `4aac1c8` |
| ---------------------------------------- | -------------------- |
| whole-run throughput (ops/s)             | **8.64**             |
| steady-state throughput (ops/s, 60 s win)| 2.0                  |
| p50 / p95 / p99 completion latency (ms)  | 3 004.5 / 3 006.8 / 3 007.6 |
| missed-deadline % (submit→complete)      | 4.18 %               |
| RSS slope (MB/min)                       | −2.61                |
| lease-renewal overhead (%)               | 0.00018              |

Samples = 1 (single long run per protocol). Apalis/faktory/baseline not
ported for this scenario.

## Scenario 4 — linear 10-node flow DAG (100 flows × 10 nodes)

| system     | throughput (flows/s) | p50 total (ms) | p99 total (ms) |
| ---------- | -------------------- | -------------- | -------------- |
| flowfabric | **17.01**            | **474.74**     | **5 816.08**   |
| apalis     | 9.34 (10.71 s mean wall, N=5) | — (not surfaced) | — |
| faktory 🚫 | _no DAG primitive_   | —              | —              |
| baseline 🚫| _no primitive_       | —              | —              |

flowfabric notes (from `flow_dag_linear-4aac1c8.json`):
`flow_setup_p50_ms=167.37, exec_p50_ms=308.55, exec_p95_ms=5399.07,
exec_p99_ms=5644.25, stage_latency_p50_ms=21.00,
stage_latency_p95_ms=152.00, stage_latency_p99_ms=5311.00,
setup_exec_ratio=0.54`. Apalis uses `apalis-workflow::Workflow`
(sequential and_then) per issue #51 methodology.

## Scenario 5 — capability-routed claim (happy mode, 100 workers × 1 000 tasks)

| system     | throughput (ops/s) | p50 / p95 / p99 correct-claim (ms) | correct-routing rate |
| ---------- | ------------------ | ---------------------------------- | -------------------- |
| flowfabric | **1 795.7**        | 416.7 / 531.2 / 550.0              | 1.0000               |
| apalis 🚫  | _no native capability routing_     | — | —                                  |
| baseline 🚫| _no primitive_     | —                                  | —                    |

Scarce / partial modes not re-run this cycle (no prior-SHA parity for
partial/scarce on this host class; happy mode is the headline).

## Regression vs. prior SHAs

| scenario                          | prior SHA (most recent)   | prior thr  | new thr    | delta     | notes |
| --------------------------------- | ------------------------- | ---------- | ---------- | --------- | ----- |
| submit_claim_complete             | `01f6327` (2026-04-20)    | 2 171.6    | 2 966.0    | **+36.6 %** | improvement — criterion warn "unable to complete 10 samples in 30 s" at 10k size, but estimate is stable |
| suspend_signal_resume             | `01f6327` (2026-04-20)    | 105.1      | 109.8      | +4.5 %    | within noise; p99 11.162 vs 11.908 ms |
| long_running (steady-state)       | `01f6327` (2026-04-20)    | 2.0        | 2.0        | flat      | refill-bound (20 tasks / 10 s); expected floor |
| long_running (missed-deadline %)  | `01f6327`                 | 3.73 %     | 4.18 %     | +0.45 pp  | within bimodal bench variance (baseline doc: 1.98–7.04 %) |
| cap_routed-happy                  | `1b2cce5` (2026-04-17)    | 114.1 @ 20 tasks | 1 795.7 @ 1 000 tasks | n/a | scale changed 20→1 000 tasks; not directly comparable |
| submit_claim_complete-apalis      | `1b2cce5` (2026-04-17)    | 51.9       | 97.8       | +88.7 %   | apalis-redis upgrade / client tuning since April 17 |
| flow_dag-apalis                   | `1b2cce5` (approx, hand-rolled chain) | 1.02 | 9.34     | n/a       | methodology switched to `apalis-workflow::Workflow` per #51 |
| submit_claim_complete-baseline    | `0181f30` (2026-04-18)    | 14 724.3   | 14 462.2   | −1.8 %    | within Valkey ceiling noise |

No regressions exceed release-gate thresholds (FAIL: thr > 10 % drop,
p99 > 20 % drop). `submit_claim_complete` shows a material uplift since
`01f6327`; likely driven by the trait-split / observability work in the
v0.5.0 bundle.

## Benches that failed to run

None. All 8 scheduled runs completed on `4aac1c8`. One operational
incident: an inter-bench `valkey-cli FLUSHALL` wiped the HMAC
partition-seeded secrets that `ff-server` installs at boot; the
subsequent `suspend_signal_resume` iterations returned
`hmac_secret_not_initialized`. Mitigation was to restart `ff-server`
after the flush and rerun; the captured run is the clean one. Protocol
note: don't `FLUSHALL` without restarting `ff-server` (or rotating
secrets via the admin endpoint).

## Artifacts

JSON reports captured this cycle (all tagged `-4aac1c8`):

- `submit_claim_complete-4aac1c8.json`
- `suspend_signal_resume-4aac1c8.json`
- `long_running_steady_state-4aac1c8.json`
- `flow_dag_linear-4aac1c8.json`
- `cap_routed-happy-4aac1c8.json`
- `submit_claim_complete-baseline-4aac1c8.json`
- `submit_claim_complete-apalis-4aac1c8.json`
- `flow_dag-apalis-4aac1c8.json`

Prior-SHA JSON preserved in `benches/results/` (no deletions).

## How to regenerate

```
# From benches/
cargo bench --bench submit_claim_complete
cargo bench --bench suspend_signal_resume
cargo run --release --bin long_running -- --workers 100 --seed 10000 --duration 300
cargo run --release --bin flow_dag -- --flows 100 --nodes 10
cargo run --release --bin cap_routed -- --mode happy
cd comparisons/baseline && cargo run --release --bin baseline-scenario1
cd comparisons/apalis   && cargo run --release --bin apalis-scenario1
cd comparisons/apalis   && cargo run --release --bin apalis-scenario4
```
