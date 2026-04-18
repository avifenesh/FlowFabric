# FlowFabric vs. peer task queues — benchmark comparison

Side-by-side numbers across the five scenarios defined in
`benches/README.md`. Each cell is `throughput_ops_per_sec` on the first
line and `p50 / p95 / p99` latency in milliseconds on the second.

This page is filled incrementally: Phase A ships scenario 1 for
FlowFabric + the hand-rolled `baseline`. Phase B + C close the
remaining cells.

## Scenario 1 — submit → claim → complete throughput

| system      | 1 000 tasks          | 10 000 tasks         | 100 000 tasks |
| ----------- | -------------------- | -------------------- | ------------- |
| flowfabric  | _pending first run_  | _pending first run_  | _Phase B large-size toggle_ |
| baseline    | _pending first run_  | _pending first run_  | _Phase B_     |
| apalis      | _Phase C_            | _Phase C_            | _Phase C_     |
| faktory     | _Phase C_            | _Phase C_            | _Phase C_     |

## Scenario 2 — suspend → signal → resume latency

| system          | p50 (ms)           | p95 (ms)           | p99 (ms)           | throughput (ops/s) |
| --------------- | ------------------ | ------------------ | ------------------ | ------------------ |
| flowfabric      | _Phase B (W2)_     | _Phase B (W2)_     | _Phase B (W2)_     | _Phase B (W2)_     |
| apalis 🚫       | _no native suspend_| —                  | —                  | —                  |
| faktory 🚫      | _no native suspend_| —                  | —                  | —                  |
| baseline 🚫     | _no primitive_     | —                  | —                  | —                  |

**Why apalis / faktory / baseline have no number here.** FlowFabric's
`task.suspend()` releases the lease and mints an HMAC-bound waitpoint
token in a single Lua call; the execution sits in `suspended` state
and resumes when the signed signal arrives (RFC-004). Apalis has no
equivalent: the closest shape is a retry-middleware workaround (job
errors with a backoff, polls Redis for a signal key on each retry
tick). Investigated during Phase B/C but NOT shipped — the retry-poll
floor (~100 ms × retry interval) measures a fundamentally different
primitive (polled retries, unauthenticated signal keys), not a
suspend/resume comparison. Implementing it would publish a number
that misleads readers into treating the systems as comparable here.
Faktory has no middleware story for this at all; baseline (raw
redis-rs) has no lease semantics.

Recorded as a differentiator: the cell is structurally "FlowFabric
only".

## Scenario 3 — 5-minute steady-state, 100 workers, 10 000 tasks

_Phase B (W2)._

## Scenario 4 — linear 10-node flow DAG

| system     | throughput (flows/s) | p50 total (ms) | p99 total (ms) |
| ---------- | -------------------- | -------------- | -------------- |
| flowfabric | _fill from flow_dag_linear-<sha>.json_ | _…_ | _…_ |
| apalis 🚫  | _no DAG primitive_   | —              | —              |
| faktory 🚫 | _no DAG primitive_   | —              | —              |
| baseline 🚫| _no primitive_       | —              | —              |

The flow_dag report's `notes` field carries the breakdown:
`flow_setup_p50_ms`, `exec_p50_ms`, `exec_p95_ms`, `exec_p99_ms`,
`stage_latency_p50_ms`, `stage_latency_p99_ms`, `setup_exec_ratio`.
Copy the three headline numbers from
`benches/results/flow_dag_linear-<sha>.json` into the flowfabric row;
paste the notes line immediately below the table.

## Scenario 5 — capability-routed claim

_Phase B. This is FlowFabric's differentiator — apalis/faktory skip.
Expected cells: flowfabric + baseline (baseline cannot route, documents
the gap)._

## How to regenerate

```
# From benches/
cargo bench --bench submit_claim_complete          # scenario 1
cargo run --release --bin flow_dag -- --flows 100 --nodes 10   # scenario 4
cd comparisons/baseline && cargo run --release --bin baseline-scenario1
```

Then copy the `throughput_ops_per_sec` and `latency_ms` triples from
each scenario's JSON in `benches/results/` into the tables above.

Automation for this will land with a subsequent PR once ≥ 4 scenarios
have FlowFabric-side numbers; hand-written for now to keep the schema
work focused on the bench runners themselves.
