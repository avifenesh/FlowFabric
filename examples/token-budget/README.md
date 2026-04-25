# token-budget — flow-level token budgets with per-attempt accounting

End-to-end FlowFabric example: a `batch-inference` runner dispatches
five LLM prompts as child executions of one flow under a shared
**per-flow token budget**. Each worker reports usage in real time via
`report_usage`; when the budget's hard limit is breached the submitter
cancels the flow with `cancel_pending` so in-flight executions drain
while new work is rejected.

**Proved live.** 2026-04-24 against Valkey 8.x + `ff-server` 0.9 (main
`a5a019d`); full run in ~1.0 s wall-clock from submit → hard-budget
cancel → terminal. Transcript in [Expected transcript](#expected-transcript).

---

## Why this example exists

This is UC-37 (per-attempt token / cost accounting) + UC-39
(flow-level budget enforcement) — two of the FlowFabric primitives
that competing orchestrators do not ship. See
[`docs/pre-rfc-use-cases-and-primitives.md`](../../docs/pre-rfc-use-cases-and-primitives.md)
for the full UC inventory.

The scenario is deliberately realistic:

* A batch of prompts is dispatched, each as its own execution
  (UC-29 fan-out).
* Every attempt reports `(input_tokens, output_tokens, cost_micros)`
  deltas against a shared budget as it runs (UC-37 real-time
  accounting — not post-hoc roll-up).
* The submitter polls budget status; at the soft threshold it logs
  a warning, at the hard threshold it cancels the flow (UC-34
  flow-scoped failure policy).
* Post-mortem: `describe_execution` per child, including v0.9's
  `LeaseSummary` snapshot fields (`lease_id`, `attempt_index`,
  `last_heartbeat_at`).

The token simulator is a **seeded PRNG keyed on `execution_id.hash()`**
so re-runs reproduce byte-for-byte on the same flow — essential for
using this example as a release-gate live-validation surface
(CLAUDE.md §5).

---

## UC grid

| UC     | Surface                                                 | Callsite in `src/main.rs`     |
|--------|---------------------------------------------------------|-------------------------------|
| UC-29  | Fan-out: one flow, N children                           | `create_flow_and_members`     |
| UC-34  | Flow-scoped failure policy: `cancel_pending` on breach  | `poll_and_enforce_budget`     |
| UC-37  | Real-time per-attempt token accounting                  | `run_worker_loop`             |
| UC-39  | Whole-flow shared max-token + max-cost budget           | `create_flow_budget`          |

v0.9 surface exercised naturally by the scenario (not narrated):

| Surface                                 | v0.9 issue | Callsite                                      |
|-----------------------------------------|------------|-----------------------------------------------|
| `flowfabric` umbrella crate             | #279       | `Cargo.toml` — single dep                     |
| `backend.prepare()`                     | #281       | `main` (boot)                                 |
| `backend.seed_waitpoint_hmac_secret`    | #280       | `main` (boot)                                 |
| `LeaseSummary { lease_id, attempt_index, last_heartbeat_at }` | #278 | `print_post_mortem` |
| `flowfabric::sdk::ClaimGrant`           | #283       | Worker's `claim_via_server` return type       |

---

## Prereqs

1. **Valkey ≥ 8.x** on `localhost:6379`:
   ```bash
   docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
   ```
   Or a natively running `valkey-server`.

2. **Rust stable** + `cargo`.

3. **`ff-server`** (the FlowFabric HTTP control plane). Build and run:
   ```bash
   cargo build -p ff-server --release
   HMAC=$(openssl rand -hex 32)
   FF_WAITPOINT_HMAC_SECRET=$HMAC \
     ./target/release/ff-server &
   # ff-server listens on 0.0.0.0:9090 by default
   ```
   Keep the `$HMAC` value — the example re-supplies it so
   `seed_waitpoint_hmac_secret` returns `AlreadySeeded { same_secret: true }`
   on subsequent runs.

No LLM API keys required — the token-cost model is simulated with a
deterministic seeded RNG. Real LLM integration is
[`llm-race`](../llm-race)'s job; this example's focus is the budget
primitive.

---

## Run it

From the repo root:

```bash
cd examples/token-budget
HMAC=$(cat /tmp/ff-hmac.txt)   # the value you set when starting ff-server
FF_WAITPOINT_HMAC_SECRET=$HMAC cargo run --release
```

Exits `0` after hard-breach cancel + post-mortem; exits non-zero on any
backend failure or 60-second demo-budget overrun.

### Env vars

| Variable                     | Default                  | Purpose                                              |
|------------------------------|--------------------------|------------------------------------------------------|
| `FF_HOST`                    | `localhost`              | Valkey host                                          |
| `FF_PORT`                    | `6379`                   | Valkey port                                          |
| `FF_SERVER_URL`              | `http://localhost:9090`  | ff-server HTTP API                                   |
| `FF_WAITPOINT_HMAC_SECRET`   | 64 × `"0"`               | Waitpoint signing secret (match ff-server's)         |
| `FF_WAITPOINT_HMAC_KID`      | `k1`                     | Kid to install/verify (match ff-server's default)    |
| `RUST_LOG`                   | `token_budget=info`      | Tracing filter                                       |

---

## Expected transcript

Single-run against a fresh flow (timings approximate; execution ids
are UUIDs per run):

```
INFO token_budget: waitpoint HMAC secret provisioned outcome=AlreadySeeded { kid: "k1", same_secret: true }
INFO token_budget: flow budget created budget_id=<uuid> flow_id=<uuid> hard_tokens=1200
INFO token_budget: flow created flow_id=<uuid> partition=<0-255> children=5
INFO token_budget: child execution enqueued execution_id=<eid> index=0
INFO token_budget: child execution enqueued execution_id=<eid> index=1
INFO token_budget: child execution enqueued execution_id=<eid> index=2
INFO token_budget: child execution enqueued execution_id=<eid> index=3
INFO token_budget: child execution enqueued execution_id=<eid> index=4
INFO token_budget: worker loop started
INFO token_budget: simulated inference complete; reporting usage execution_id=<eid> attempt=0 input_tokens=<50-150> output_tokens=<100-400> cost_micros=<N>
INFO token_budget: usage reported; within budget execution_id=<eid>
... (further per-child reports; some SoftBreach, some HardBreach)
WARN token_budget: soft limit approaching — flow will reject new claims on hard breach tokens=<N> cost_micros=<N> soft_tokens=800 soft_cost_micros=10000
WARN token_budget: hard budget breached — cancelling flow tokens=<N> cost_micros=<N> hard_tokens=1200 hard_cost_micros=15000
INFO token_budget: cancel_flow returned result=Cancelled { cancellation_policy: "cancel_pending", member_execution_ids: [...] }
INFO token_budget: ── post-mortem: per-child LeaseSummary ──
INFO token_budget: child snapshot (no active lease — terminal or never leased) execution_id=<eid> state=Completed
INFO token_budget: child snapshot (no active lease — terminal or never leased) execution_id=<eid> state=Completed
INFO token_budget: child snapshot (no active lease — terminal or never leased) execution_id=<eid> state=Failed
INFO token_budget: child snapshot (no active lease — terminal or never leased) execution_id=<eid> state=Failed
INFO token_budget: child snapshot (no active lease — terminal or never leased) execution_id=<eid> state=Failed
INFO token_budget: final budget status final_tokens=<N> final_cost_micros=<N> hard_limit_tokens=1200 breach_count=<3-4> soft_breach_count=<0-1>
INFO token_budget: demo complete outcome=HardBreachCancelled
```

Notes on the exact numbers:

* The seeded PRNG means a *repeat* run against the same set of
  execution ids produces byte-identical token counts. But each run
  mints fresh `FlowId` / `ExecutionId` UUIDs, so the totals vary
  across runs within the shape `input_tokens ∈ [50, 150]`,
  `output_tokens ∈ [100, 400]`.
* `state=Completed` for the 1–2 children whose `report_usage` returned
  `Ok` or `SoftBreach` before the hard-breach mark. `state=Failed` for
  children whose `report_usage` returned `HardBreach` (they call
  `task.fail` with `error_category = "budget_hard_breach"`).
* The active `LeaseSummary` is `None` by post-mortem time because all
  children are terminal; Valkey atomically clears lease fields on
  terminal transition. To see populated `LeaseSummary.lease_id` /
  `attempt_index` / `last_heartbeat_at`, pause the worker before
  cancel_flow and `curl /v1/executions/<eid>` mid-flight.

---

## What to inspect during / after a run

* **Budget status (HTTP):**
  ```bash
  curl -s http://localhost:9090/v1/budgets/<budget-id> | jq
  ```
  Shape matches [`BudgetStatus`](../../crates/ff-core/src/contracts/mod.rs):
  `usage`, `hard_limits`, `soft_limits`, `breach_count`,
  `soft_breach_count`.

* **Describe a terminal child:**
  ```bash
  curl -s http://localhost:9090/v1/executions/<eid> | jq
  ```

* **Re-run is idempotent.** `seed_waitpoint_hmac_secret` returns
  `AlreadySeeded { same_secret: true }` on repeat runs against the
  same ff-server with the same `FF_WAITPOINT_HMAC_SECRET`. Fresh budget
  + flow ids are minted each run so the hard-limit trip is reproducible.

---

## Architecture

```
main (single binary)
  │
  ├─ boot:   backend.prepare() + seed_waitpoint_hmac_secret()    (v0.9 #281, #280)
  ├─ setup:  backend.create_budget(scope=Flow, hard=1200 tokens) (UC-39)
  │          + HTTP POST /v1/flows   + POST /v1/executions × 5  (UC-29 fan-out)
  │          + POST /v1/flows/{id}/members × 5
  │
  ├─ spawn worker task (same process) — claims each child:
  │    │ claim_via_server
  │    │ simulate_inference (seeded PRNG → tokens)
  │    │ task.report_usage(budget, [(tokens, N), (cost_micros, N)])  (UC-37)
  │    │ match ReportUsageResult:
  │    │   Ok         → task.complete()
  │    │   SoftBreach → task.complete()   + warn log
  │    │   HardBreach → task.fail()       + warn log (budget_hard_breach)
  │
  └─ poll loop (submitter):
       │ backend.get_budget_status(budget_id)
       │ soft threshold (>= soft_limit): log once
       │ hard threshold (tokens >= hard_limit OR breach_count > 0):
       │   backend.cancel_flow(CancelPending, NoWait)               (UC-34)
       │   → returns Cancelled { member_execution_ids }
       │
       │ post-mortem: describe_execution per child, print
       │   ExecutionSnapshot { public_state, current_lease: LeaseSummary{..} }  (v0.9 #278)
```

---

## Files

```
examples/token-budget/
├── Cargo.toml              # flowfabric umbrella pin only
├── README.md               # this file
└── src/
    └── main.rs             # ~400 lines: submit + worker + post-mortem
```

---

## Known surface notes

* The example intentionally **does not** call `cancel_flow` with
  `CancelFlowWait::WaitIndefinite` — that variant returns
  `EngineError::Unavailable` on the current Valkey backend (only
  `NoWait` and `WaitTimeout` are wired through `ff_cancel_flow`
  today). `cancel_pending` + `NoWait` is sufficient for the UC: the
  Valkey cancel path dispatches each member cancel asynchronously and
  the submitter's post-mortem poll observes terminal state within a
  few hundred ms.

* `LeaseSummary` fields populate only while an execution holds a
  lease. The example's deterministic per-attempt path is fast enough
  that by the time `describe_execution` runs every child is terminal,
  so the current_lease is `None`. This is not a gap — it is the v0.9
  #278 contract (see `LeaseSummary` docs in `ff-core/src/contracts`).
  To demo the populated path, lower `max_concurrent_tasks` to 1, add a
  `sleep` in `simulate_inference`, and `describe_execution` while the
  worker is still running.

* The umbrella crate re-exports the full SDK surface. There is
  nothing in this example that required reaching past
  `flowfabric::sdk::*` or `flowfabric::core::*` — which is the v0.9
  ergonomic promise and a partial validation that #279's coverage is
  complete for the budget primitive.
