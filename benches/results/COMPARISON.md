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

_Phase B_.

## Scenario 3 — 5-minute steady-state, 100 workers, 10 000 tasks

_Phase B_.

## Scenario 4 — linear 10-node flow DAG

_Phase B_.

## Scenario 5 — capability-routed claim

_Phase B. This is FlowFabric's differentiator — apalis/faktory skip.
Expected cells: flowfabric + baseline (baseline cannot route, documents
the gap)._

## How to regenerate

```
cd benches
cargo bench --bench submit_claim_complete
cd comparisons/baseline && cargo run --release --bin baseline-scenario1
```

Then copy the two `throughput_ops_per_sec` and `latency_ms` triples
from `benches/results/submit_claim_complete-<sha>.json` and
`benches/results/submit_claim_complete-baseline-<sha>.json` into the
scenario-1 table above.

Automation for this will land with Phase B once we have >1 scenario to
aggregate; hand-written for now.
