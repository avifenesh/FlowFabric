# FlowFabric vs. peer task queues — benchmark comparison

Side-by-side numbers across the five core scenarios. Each cell is
`throughput_ops_per_sec` on the first line and `p50 / p95 / p99`
latency in milliseconds on the second.

Prior FF(Valkey) + apalis + baseline numbers: HEAD `4aac1c8` (workspace
v0.5.0 bump). Host class: AMD EPYC 9R14 × 16, Valkey 7.2.4.

**2026-04-28 apalis refresh (HEAD `66ab76b`, workspace v0.11.0).** Per
apalis maintainer @geofmureithi's tuning guidance on issue #51 the
apalis harness now uses `RedisConfig { poll_interval: 5ms,
buffer_size: 100 }`, spawns N=16 independent `WorkerBuilder` instances
(not `.concurrency(16)`), and layers `.parallelize(tokio::spawn)`.
Same-day baseline re-run: 14 654 ops/s (prior 14 462 — within noise).
Same-day FF(Valkey) re-runs: scenario 1 = 3 036 ops/s (prior 2 966 —
+2% within criterion 10-sample noise), scenario 4 = 17.46 flows/s
(prior 17.01 — within noise). Host is steady vs v0.5.0 measurement
epoch.

Postgres (new): HEAD `a1c2322` (workspace v0.6.1, Wave 7c on v0.7
track). Host class: AMD EPYC 9R14 × 16, PostgreSQL 16 with
`max_locks_per_transaction = 512` (default 64 is insufficient — see
Phase 0 RFC §"Host tuning"). See `benches/results/*-pg-a1c2322.json`
for full configs and notes.

**Port-shape caveat.** The Postgres benches call
`PostgresBackend::from_pool` + the `EngineBackend` trait directly
(analogous to `benches/comparisons/ferriskey-baseline` on the Valkey
side, NOT the full ff-sdk/ff-server path). The column labelled
`FF(Postgres)` therefore compares most cleanly to `baseline` +
`ferriskey-baseline`; comparisons against the full-stack `FF(Valkey)`
column overstate the PG back-pressure since PG traverses the full
engine on every claim/complete pair. See the v0.7 Phase 0 RFC for the
full discussion.

## Scenario 1 — submit → claim → complete throughput (10 000 tasks)

| system         | throughput (ops/s) | p50 / p95 / p99 (ms)      |
| -------------- | ------------------ | ------------------------- |
| FF(Valkey)     | **3 036**          | — (criterion agg)         |
| FF(Postgres)   | **510**            | 24.60 / 39.41 / 44.10     |
| apalis         | **1 419**          | — (noop, sub-µs)          |
| baseline       | 14 654             | — (sampled post-refresh)  |
| faktory        | _Phase C_          | _Phase C_                 |

*FF(Postgres) 16 workers, 10 k tasks, direct EngineBackend::claim +
complete. ~5.8× slower than FF(Valkey) full-stack and ~28× slower than
the raw-Valkey baseline. The lane-level contention is the dominant
term: every claim acquires FOR UPDATE SKIP LOCKED on ff_exec_core, the
complete rewrites ff_exec_core + writes ff_completion_event (outbox
trigger fires NOTIFY). At 16 concurrent workers the shared lock table
saturates without the 512-lock bump.*

## Scenario 2 — suspend → signal → resume latency

| system          | p50 (ms)           | p95 / p99 (ms)     | throughput (ops/s) |
| --------------- | ------------------ | ------------------ | ------------------ |
| FF(Valkey)      | **9.11**           | 9.99 / 11.16       | 109.8              |
| FF(Postgres)    | _skipped — see note_ | —                | —                  |
| apalis          | _no native suspend_| —                  | —                  |
| faktory         | _no native suspend_| —                  | —                  |
| baseline        | _no primitive_     | —                  | —                  |

*FF(Postgres) scenario 2 skipped in this Phase 0 cut. The trio
(suspend / deliver_signal / claim_resumed_execution) is implemented
in-crate at SERIALIZABLE with a 3-attempt retry; the integration test
in `crates/ff-backend-postgres/tests/suspend_signal.rs` covers parity.
Driving the ingress contract (RFC-014 Pattern-3 SuspendArgs +
WaitpointBinding + ResumeCondition + ResumePolicy +
SuspensionRequester + TimeoutBehavior + idempotency-key plumbing) at
the bench layer duplicates ~150 lines of server-side plumbing that's
still shifting; the cut is deferred to v0.7 Phase 1.*

## Scenario 3 — steady-state (workers × time)

| system         | config                   | throughput (ops/s) | total completed |
| -------------- | ------------------------ | ------------------ | --------------- |
| FF(Valkey)     | 100 workers × 5 min      | **8.64** whole-run | —               |
| FF(Postgres)   | 16 workers × 2 min       | **604.8**          | 72 574          |

*Config differs deliberately. FF(Postgres) uses 16 workers because the
Valkey-side 100-worker config exhausts the Postgres 16 pooled
connections + triggers `max_locks_per_transaction` at 100 concurrent
claim txs even at the 512-bumped setting; 16 matches scenarios 1/4.
Duration halved to 120 s to stay under the 6 h watchdog for a single
bench pass. FF(Valkey)'s 5-min run is refill-bound and measures
long-tail latency drift that a 120 s window can't surface — the two
numbers ARE NOT directly comparable. FF(Postgres)'s 604.8 ops/s is the
steady-state claim/complete throughput; FF(Valkey)'s 8.64 ops/s is the
whole-run average dominated by the 5-min-long refill interval. A
v0.7 Phase 1 re-run will align the configs.*

## Scenario 4 — linear 10-node flow DAG

| system         | throughput (flows/s) | p50 / p99 total (ms per flow) |
| -------------- | -------------------- | ----------------------------- |
| FF(Valkey)     | **17.46**            | 412.88 / 5 446.69             |
| FF(Postgres)   | **48.2**             | 760 / 882 (approx, per-flow)  |
| apalis         | **10.2**             | — (not surfaced)              |
| faktory        | _no DAG primitive_   | —                             |
| baseline       | _no primitive_       | —                             |

*FF(Postgres) emulates the 10-node chain via 10 claim+complete ops per
flow on a pooled lane. create_flow + stage_edge_dependency +
apply_dependency_to_child are Valkey-side FCALLs not exposed on the
PG EngineBackend surface (they sit at the ff-server ingress layer). The
per-flow latency is the sum of 10 per-op latencies on an interleaved
pool — it measures end-to-end chain wall but without the dependency
resolution overhead the FF(Valkey) bench exercises. Direct comparison
overstates PG on throughput (no dep-resolution cost here) and
understates PG on per-flow latency (no per-stage edge eval). A v0.7
Phase 1 re-run lands after the flow-ingress FCALLs port to PG.*

## Scenario 5 — capability-routed claim (happy mode)

| system         | config                      | throughput (ops/s) | p50 / p95 / p99 (ms) |
| -------------- | --------------------------- | ------------------ | -------------------- |
| FF(Valkey)     | 100 workers × 1 000 tasks   | **1 795.7**        | 416.7 / 531.2 / 550.0 |
| FF(Postgres)   | 40 workers × 10 000 tasks   | **300.0**          | 23.36 / 27.80 / 29.06 |
| apalis         | _no native cap routing_     | —                  | —                    |
| baseline       | _no primitive_              | —                  | —                    |

*FF(Postgres): 10 capability sets × 4 workers/set × 1 000 tasks/set =
10 k tasks total. Capability subset-match runs in Rust after the FOR
UPDATE SKIP LOCKED scan (see `crates/ff-backend-postgres/src/attempt.rs`
module comment). Lower throughput than FF(Valkey) reflects the 40-vs-100
worker gap + PG's per-claim tx cost; lower p99 reflects the
cap-filtering fast path (no Lua, single scan-and-filter).*

## Scenario 6 / quorum-cancel fan-out (RFC-016 §4.2 SLO gate)

| system         | n=10 p99 (ms) | n=100 p99 (ms)  | n=1000 p99 (ms) | SLO at n=100 |
| -------------- | ------------- | --------------- | --------------- | ------------ |
| FF(Valkey)     | _prior gate_  | _prior gate_    | _prior gate_    | _prior gate_ |
| FF(Postgres)   | **67.83**     | **64.08**       | **186.04**      | **PASS (≤500)** |

*FF(Postgres) via `cancel_flow` SERIALIZABLE cascade (one tx flips flow
+ every non-terminal member, 3-attempt retry on serialization
failures). At n=100 the cascade lands at p99=64 ms — comfortable ~7.8×
margin against the 500 ms SLO. avg_members_cancelled=100 verifies the
cascade actually fires (earlier draft had a partition-key mismatch
that returned in 0.5 ms with zero members cancelled; fixed by seeding
into `flow_partition(flow_id, PartitionConfig::default()).index`).
Note: this is an approximation of the full AnyOf/Quorum +
CancelRemaining path — create_flow + stage_edge_dependency +
apply_dependency_to_child are Valkey-side FCALLs that the Postgres
port has not yet exposed. cancel_flow's SERIALIZABLE cascade is a
conservative upper bound on what the full path will cost once the
ingress FCALLs land on PG.*

## Postgres column — how to regenerate

Prereqs:
- Postgres 16 reachable; `ALTER SYSTEM SET max_locks_per_transaction = 512` + restart.
- `DATABASE_URL=postgres://postgres:postgres@localhost/ff_bench_v0_7` (or `FF_PG_TEST_URL`).

```
cd benches/comparisons/postgres
cargo build --release
# Reset + run per scenario (drop+create keeps the partitioned tables
# well-conditioned between samples; migrations re-applied on each
# bench start via apply_migrations).
psql -U postgres -c 'DROP DATABASE IF EXISTS ff_bench_v0_7'
psql -U postgres -c 'CREATE DATABASE ff_bench_v0_7'
DATABASE_URL=$DATABASE_URL ./target/release/pg-scenario1           --tasks 10000
DATABASE_URL=$DATABASE_URL ./target/release/pg-scenario3           --duration-secs 120
DATABASE_URL=$DATABASE_URL ./target/release/pg-scenario4           --flows 100 --workers 16
DATABASE_URL=$DATABASE_URL ./target/release/pg-scenario5
DATABASE_URL=$DATABASE_URL ./target/release/pg-quorum-cancel-fanout --samples 10
```

## Artifacts (Postgres, tagged `-pg-a1c2322`)

- `submit_claim_complete-pg-a1c2322.json`
- `long_running-pg-a1c2322.json`
- `flow_dag-pg-a1c2322.json`
- `cap_routed-pg-a1c2322.json`
- `quorum_cancel_fanout-n10-pg-a1c2322.json`
- `quorum_cancel_fanout-n100-pg-a1c2322.json`
- `quorum_cancel_fanout-n1000-pg-a1c2322.json`

All gitignored. Markdown only in-repo; rerun the commands above for
local reproduction.
