# RFC-002: Attempt Model and Execution Lineage

**Status:** Draft
**Author:** FlowFabric Team (Worker-3)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 1.5, Section 3.4, Finalization items 3 and 4

---

## Summary

An **attempt** is a durable sub-record of an execution representing one concrete runnable try. A single execution may accumulate many attempts over its lifetime through retries, reclaims, replays, and fallback progressions. This RFC defines the attempt object, its lifecycle, invariants, lineage semantics, per-attempt policy snapshots, and Valkey data model. The attempt is what makes execution history concrete and debuggable — without it, retry/reclaim/replay history collapses into muddy metadata on the execution record.

## Motivation

Several use cases require the engine to distinguish between "what the execution is" and "what happened during each concrete run":

- **UC-06 (Retry with backoff):** Each retry is a new attempt with its own timing, outcome, and usage.
- **UC-12 (Crash recovery / stalled reclaim):** A crashed worker's attempt must be marked interrupted; the reclaiming worker gets a fresh attempt with a new lease epoch.
- **UC-16 (Manual requeue / replay):** Replaying terminal work creates a new attempt under the same execution identity, preserving full lineage.
- **UC-37 (Token / cost accounting):** Per-attempt attribution is required to explain which concrete run consumed which resources.
- **UC-38 (Model/provider fallback):** Each fallback tier progression that restarts model execution is a distinct attempt with its own provider/model snapshot.
- **UC-57 (Snapshot and diagnosis):** Operators need the concrete run timeline — not just "attempt 3 failed" but the full history of what each attempt did, who owned it, and why it ended.

Without attempts, the execution object must carry accumulated retry/reclaim state directly, leading to ambiguous timestamps, unclear ownership lineage, and no clean way to attribute usage or stream output to the concrete run that produced them.

## Detailed Design

### Object Definition

An attempt record contains the following fields, grouped by purpose.

#### Identity

| Field | Type | Required | Description |
|---|---|---|---|
| `attempt_id` | `UUID` | Yes | Globally unique identifier for this attempt. Used for audit correlation; `attempt_index` is the primary binding key. |
| `execution_id` | `UUID` | Yes | Stable identity of the parent execution (RFC-001). |
| `attempt_index` | `u32` | Yes | Zero-based monotonically increasing index within the execution. Retry, reclaim, replay, and fallback all increment this. |

#### Timing

| Field | Type | Required | Description |
|---|---|---|---|
| `created_at` | `i64` (ms epoch) | Yes | When this attempt record was created. |
| `started_at` | `i64` (ms epoch) | No | When a worker began executing this attempt (lease acquired). `None` if the attempt was superseded before starting. |
| `ended_at` | `i64` (ms epoch) | No | When this attempt reached a terminal state. |
| `suspended_at` | `i64` (ms epoch) | No | When this attempt was paused by intentional suspension (RFC-004). Cleared on resume. |
| `resumed_at` | `i64` (ms epoch) | No | When this attempt resumed after suspension. Set on `claim_resumed_execution`. Not set for initial claims or reclaims. |
| `suspension_id` | `UUID` | No | Suspension episode that paused this attempt. Set by `suspend_attempt`, cleared by `resume_attempt`. Used for correlation between attempt and suspension records. |

#### Outcome

| Field | Type | Required | Description |
|---|---|---|---|
| `attempt_state` | `AttemptState` enum | Yes | Current lifecycle state of this attempt (see Lifecycle section). |
| `failure_reason` | `String` | No | Structured failure reason if `ended_failure`. |
| `failure_category` | `String` | No | Machine-readable failure category (e.g., `timeout`, `worker_error`, `provider_error`, `budget_exceeded`). |
| `terminalized_by_override` | `bool` | No | `true` if an operator or privileged action forced the outcome. Set by `cancel_execution` when `source = operator_override`, and by `expire_execution` (engine-forced timeout). Enables audit queries: "which attempts were terminated by the system vs the worker?" |

#### Ownership / Placement

| Field | Type | Required | Description |
|---|---|---|---|
| `lease_id` | `String` | No | Identity of the lease acquired for this attempt (RFC-003). Present when attempt reaches `started`. |
| `lease_epoch` | `u64` | No | Monotonic fencing token from the lease (RFC-003). Increments on every new lease issuance; preserved on renew. Used to reject stale mutations. |
| `worker_id` | `String` | No | Logical identity of the worker that claimed this attempt. |
| `worker_instance_id` | `String` | No | Instance-level identity (e.g., process ID, container ID) for disambiguation when the same logical worker restarts. |
| `route_snapshot` | `RouteSnapshot` | No | Captured routing decision at claim time: selected lane, pool, locality. |

#### AI-Specific Context

| Field | Type | Required | Description |
|---|---|---|---|
| `fallback_index` | `u32` | No | Position in the execution's fallback chain that this attempt used. `0` = primary provider/model. |
| `provider_model_snapshot` | `ProviderModelSnapshot` | No | Which provider and model this attempt was routed to. |
| `usage` | `UsageSummary` | No | Token counts, cost, and latency accumulated during this attempt. |

The `UsageSummary` structure:

| Field | Type | Description |
|---|---|---|
| `input_tokens` | `u64` | Input/prompt tokens consumed. |
| `output_tokens` | `u64` | Output/completion tokens consumed. |
| `thinking_tokens` | `u64` | Thinking/reasoning tokens consumed (model-dependent). |
| `total_tokens` | `u64` | Sum of all token types. |
| `cost_microdollars` | `u64` | Cost in microdollars (1 USD = 1,000,000). Avoids floating point. |
| `latency_ms` | `u64` | Wall-clock latency from `started_at` to `ended_at`. |
| `time_to_first_token_ms` | `u64` | Latency to first stream frame, if applicable. |

#### Lineage

| Field | Type | Required | Description |
|---|---|---|---|
| `attempt_type` | `AttemptType` enum | Yes | Why this attempt was created: `initial`, `retry`, `reclaim`, `replay`, `fallback`. |
| `retry_reason` | `String` | No | Why a retry was triggered (e.g., `retryable_error`, `timeout`). Present when `attempt_type = retry`. |
| `reclaim_reason` | `String` | No | Why reclaim occurred (e.g., `lease_expired`, `worker_crash_detected`). Present when `attempt_type = reclaim`. |
| `replay_reason` | `String` | No | Why replay was requested. Present when `attempt_type = replay`. |
| `replay_requested_by` | `String` | No | Actor identity that requested replay (operator ID, system policy, API caller). |
| `replayed_from_attempt_index` | `u32` | No | Which prior attempt this replay follows. Provides explicit lineage. |
| `previous_attempt_index` | `u32` | No | The attempt this one directly follows. Always `attempt_index - 1` when `attempt_index > 0`. |

#### Policy Snapshot

| Field | Type | Required | Description |
|---|---|---|---|
| `policy_snapshot` | `AttemptPolicySnapshot` | Yes | Frozen policy context for this attempt (see AttemptPolicySnapshot section). |

### Invariants

#### Invariant A1 — Execution identity is stable across all attempt types

Attempts do not replace execution identity. They refine it. A retry, reclaim, replay, or fallback progression all operate on the same `execution_id`. The execution object (RFC-001) is the stable identity; attempts are the concrete run records beneath it.

**Rationale:** Stable execution identity is the foundation of the entire model. Without it, operators lose the ability to trace the full history of one logical unit of work. Clients that submitted the execution can always query by `execution_id` regardless of how many attempts have occurred.

#### Invariant A2 — Attempt ordering is monotonic

Each new attempt gets a strictly increasing `attempt_index` within its execution, regardless of whether it came from retry, reclaim, replay, or fallback. The index is zero-based and gap-free.

**Rationale:** Monotonic ordering provides a total order over the concrete run timeline. Combined with `attempt_type`, this answers both "which attempt came when" and "why it was created." Gap-free indexing simplifies range queries and sorted-set operations in Valkey.

#### Invariant A3 — One active attempt per execution

At any moment, an execution may have at most one attempt in the `started` state. Creating a new attempt requires that all previous attempts have reached a terminal state.

**Rationale:** This is the attempt-layer expression of the single-active-owner invariant (E3 from RFC-001, L1 from RFC-003). Without it, two workers could run two attempts concurrently for the same execution, producing split-brain results and double-counted usage.

#### Invariant A4 — Lease belongs to an attempt

The current lease (RFC-003) attaches to the current active attempt, not just to the execution generically. The primary binding key is `attempt_index` (monotonic, used in Valkey keys). The `lease_id` and `lease_epoch` on the attempt record must also match the active lease. `attempt_id` is recorded for audit correlation but is not used as the binding key. When a new attempt is created via reclaim, a new lease with a higher `lease_epoch` is issued.

**Rationale:** Lease-to-attempt binding is what makes stale-owner rejection work at the attempt level. If a worker holds a lease from attempt N and the execution has moved to attempt N+1 (via reclaim), the stale worker's mutations are rejected because its `lease_epoch` is outdated. Using `attempt_index` as the primary binding key (rather than `attempt_id`) simplifies Valkey key construction and validation.

#### Invariant A5 — Usage attribution is attempt-aware

Latency, token counts, cost, fallback selection, route choice, and stream output are attributed to the specific attempt that produced them. Execution-level usage summaries are derived by aggregating across attempts.

**Rationale:** Without attempt-level attribution, a retry that succeeds after a failed attempt would report combined usage — making cost analysis, provider comparison, and latency debugging unreliable. Attempt-scoped streams (RFC-006) depend on this invariant.

### Lifecycle States

An attempt has the following lifecycle states:

```
  ┌─────────┐   ┌─────────┐   ┌───────────────┐
  │ created ├──►│ started ├──►│ ended_success │
  └─────────┘   └──┬──┬───┘   └───────────────┘
                   │  │  ▲
                   │  │  │ resume + re-claim
                   │  │  │ (same attempt continues)
                   │  ├──┼──►┌───────────────────┐
                   │  │  │   │ ended_failure     │◄─┐
                   │  │  │   └───────────────────┘  │
                   │  │  │                           │ timeout
                   │  ├──┼──►┌───────────────────┐  │ (fail)
                   │  │  │   │ ended_cancelled   │◄─┤
                   │  │  │   └───────────────────┘  │ cancel
                   │  │  │                           │
                   │  ├──┼──►┌─────────────────────────┐
                   │  │  │   │ interrupted_reclaimed   │
                   │  │  │   └─────────────────────────┘
                   │  │  │
                   │  └──┼──►┌─────────────┐
                   │     │   │ suspended   ├──┘
                   │     │   └──┬──────────┘
                   │     │      │  (not terminal — same
                   │     │      │   attempt resumes on
                   │     │      │   re-claim; OR may go
                   │     │      └──►terminal on cancel
                   │     │          or timeout-fail)
                   │     │
                   (any terminal attempt may be followed
                    by a NEW attempt — the old attempt's
                    state is never overwritten)
```

| State | Description | Terminal? |
|---|---|---|
| `created` | Attempt record exists. Worker has not yet acquired a lease and begun execution. | No |
| `started` | Worker has acquired a lease and is actively executing. At most one attempt per execution may be in this state. | No |
| `suspended` | Attempt is paused because the execution intentionally suspended (RFC-004). Lease released, no active owner, but the attempt is NOT terminal — the same attempt resumes when the execution is re-claimed after resume. This is what distinguishes suspension from crash/reclaim. | No |
| `ended_success` | Attempt completed successfully. The execution may transition to `completed`. | Yes |
| `ended_failure` | Attempt failed. The execution may retry (creating a new attempt) or become terminal `failed`. | Yes |
| `ended_cancelled` | Attempt was cancelled by operator, policy, or owner. The execution transitions to `cancelled`. | Yes |
| `interrupted_reclaimed` | Attempt was interrupted because the lease expired or was revoked, and the execution was reclaimed. A new attempt follows. | Yes |

Lineage between attempts is expressed solely on the **new** attempt via `attempt_type` and `previous_attempt_index`. The old attempt's terminal state is never overwritten — `ended_failure` stays `ended_failure` whether or not a retry follows. This keeps the attempt record immutable once terminal and avoids a double-write (terminal state + superseded marker).

**State transition rules:**
- `created` → `started`: Worker acquires lease successfully.
- `started` → `ended_success`: Worker reports successful completion with valid lease.
- `started` → `ended_failure`: Worker reports failure with valid lease, or engine detects unrecoverable error.
- `started` → `ended_cancelled`: Cancellation during active execution.
- `started` → `interrupted_reclaimed`: Lease expired or was revoked; execution enters reclaim path.
- `started` → `suspended`: Worker intentionally suspends execution (RFC-004). Lease is released atomically. The attempt is paused, not ended.
- `suspended` → `started`: Execution resumes after signal satisfaction (RFC-005). A new lease is acquired and bound to this same attempt. `lease_epoch` increments. No new attempt is created.
- `suspended` → `ended_cancelled`: Execution is cancelled while suspended. Attempt becomes terminal.
- `suspended` → `ended_failure`: Suspension timeout with `fail` behavior. Attempt becomes terminal.
- Any terminal state on the *latest* attempt may be followed by creation of a *new* attempt (the terminal attempt itself does not change state).

**Forbidden transitions:**
- Any terminal state → any other state (a terminal attempt is immutable; a new attempt is created instead).
- `created` → any terminal state directly (must go through `started` first, except if the execution is cancelled before claim — in which case no attempt record is created for that attempt).
- `started` → `created` (no rollback of lease acquisition).
- `suspended` → `created` (no rollback).
- Two attempts in `started` simultaneously for the same execution.

#### Mapping: RFC-001 Attempt State Dimension → RFC-002 Attempt Lifecycle

The execution's `attempt_state` dimension (RFC-001 Section 2.2F) maps to RFC-002 attempt lifecycle states as follows:

| RFC-001 `attempt_state` value | RFC-002 attempt lifecycle | Notes |
|---|---|---|
| `none` | No attempt record exists | Freshly submitted execution, or `skipped` execution (see below). |
| `pending_first_attempt` | Latest attempt in `created` | Attempt record created, awaiting initial claim. |
| `running_attempt` | Latest attempt in `started` | Worker has acquired lease and is executing. |
| `attempt_interrupted` | Latest attempt in `suspended` (intentional pause) OR `interrupted_reclaimed` (crash/reclaim) | Two distinct causes share this RFC-001 value. Disambiguate by checking execution `lifecycle_phase`: if `suspended` → attempt is `suspended`; if `active` with `lease_expired_reclaimable` → attempt is `interrupted_reclaimed`. |
| `pending_retry_attempt` | Latest attempt in `ended_failure`, new attempt in `created` | Previous attempt failed; retry attempt created and awaiting claim. |
| `pending_replay_attempt` | Latest attempt in terminal state, new replay attempt in `created` | Replay requested; new attempt created and awaiting claim. |
| `attempt_terminal` | Latest attempt in `ended_success`, `ended_failure`, `ended_cancelled` | The final attempt has concluded and no further attempts are pending. |

#### Skipped Executions (DAG)

When a required dependency fails in a DAG flow, downstream executions become terminal `skipped` (RFC-001 `terminal_outcome = skipped`). These executions go directly to terminal state with `attempt_state = none` and **no attempt record is ever created**. There is nothing to attempt — the execution was never eligible to run.

### When to Create a New Attempt

#### New attempt required

| Trigger | Attempt Type | Rationale |
|---|---|---|
| Retry after failure | `retry` | Each retry is a distinct concrete run with its own timing, lease, usage, and outcome. |
| Retry after retryable timeout | `retry` | Timeout treated as a failed attempt; retry creates the next one. |
| Reclaim after lease expiry | `reclaim` | The interrupted run and the reclaimed run are distinct — different workers, different lease epochs. |
| Explicit replay | `replay` | Even after terminal success or failure, replay creates a new concrete run. Execution identity is preserved. |
| Fallback progression that restarts model execution | `fallback` | Switching provider/model tier is a new concrete run with a different route snapshot and provider/model snapshot. |

#### No new attempt

| Event | Rationale |
|---|---|
| Lease renewal | Same concrete run continues. The lease is extended, not replaced. |
| Signal delivery to a suspended execution | Signals affect suspension state, not the attempt layer. |
| Operator inspection | Read-only; no state change. |
| Stream tailing | Read-only; no state change. |
| Normal signal receipt while suspended | Same execution context; no new run begins until resume + claim. |
| Progress or usage reporting | Updates the current attempt's fields; does not create a new one. |

### Retry Semantics

Retry creates a new attempt on the same execution when the current attempt ends in failure and the execution's retry policy permits another try.

**Mechanics:**
1. Current attempt transitions to `ended_failure`. This state is final — the attempt record is never modified again.
2. If retry policy allows (max retries not exhausted, failure category is retryable), the engine creates a new attempt with:
   - `attempt_index` = previous + 1
   - `attempt_type` = `retry`
   - `retry_reason` = structured reason from the failed attempt
   - `previous_attempt_index` = the failed attempt's index
   - Fresh `AttemptPolicySnapshot` (route and admission may be re-resolved)
4. Execution transitions to `delayed` (if backoff applies) or `waiting` (if immediate retry).
5. A worker claims the execution, acquiring a new lease that binds to the new attempt.

**Attempt index is the retry counter.** The execution does not need a separate `retry_count` field — it is derivable as the count of attempts with `attempt_type = retry`. However, the execution record (RFC-001) may cache `current_attempt_index` for fast access.

**Backoff delay:** The delay between attempts is governed by the execution's retry policy (fixed, exponential, custom). The attempt is created immediately; the execution enters `delayed` state with an `eligible_after` timestamp. The attempt remains in `created` until a worker claims it.

### Reclaim Semantics

Reclaim creates a new attempt when the engine detects that the current attempt's lease expired without the worker completing, failing, or releasing.

**Mechanics:**
1. Engine's reclaim scanner or lease-expiry watcher detects an expired lease.
2. Current attempt transitions to `interrupted_reclaimed`.
3. Engine creates a new attempt with:
   - `attempt_index` = previous + 1
   - `attempt_type` = `reclaim`
   - `reclaim_reason` = `lease_expired` or `lease_revoked`
   - `previous_attempt_index` = the interrupted attempt's index
   - Fresh `AttemptPolicySnapshot`
4. A new lease is issued with a **higher `lease_epoch`** (RFC-003). This is what fences out the stale worker.
5. Execution transitions to `waiting` for a new worker to claim.
6. The new worker acquires the lease, and the new attempt transitions to `started`.

**`lease_epoch` increment:** The `lease_epoch` is monotonic per execution. Reclaim always increments it. Any mutation from the old worker carrying the old `lease_epoch` is rejected. This is the core mechanism that prevents stale-owner corruption.

### Replay Semantics

Replay is an explicit operator or system action that re-executes work even after the execution reached a terminal state (`completed`, `failed`, `cancelled`, `expired`).

**Locked decision:** Replay does NOT create a new execution. It creates a new attempt on the same logical execution, preserving full history under one `execution_id`.

**Mechanics:**
1. Operator or system policy invokes `replay_execution(execution_id, replay_spec)`.
2. The execution's terminal state is recorded but the execution transitions back to `waiting` (or `delayed` if replay policy specifies a delay).
3. Engine creates a new attempt with:
   - `attempt_index` = previous max + 1
   - `attempt_type` = `replay`
   - `replay_reason` = operator-provided or policy-derived reason
   - `replay_requested_by` = actor identity
   - `replayed_from_attempt_index` = the index of the terminal attempt being replayed
   - Fresh `AttemptPolicySnapshot`
4. The previously terminal attempt is *not* modified — its state remains `ended_success`, `ended_failure`, etc. The new attempt is what carries replay forward.

**Replay metadata on execution:**
The execution record (RFC-001) tracks:

| Field | Type | Description |
|---|---|---|
| `replay_count` | `u32` | Number of times this execution has been replayed. Derivable from attempt history but cached for fast access. |

Replay reason and requester are carried as transient fields (`pending_replay_reason`, `pending_replay_requested_by`) on exec core during the claim window (set by `replay_execution`, consumed and cleared by `claim_execution` which copies them to the new attempt's `replay_reason` and `replay_requested_by` fields). Persistent `last_replay_*` fields are not needed on exec core — query the latest replay attempt (filter `attempt_type = replay` in the attempts ZSET) for the most recent replay reason and requester.

**Replay guards:**
- Replay is only valid on terminal executions. Attempting to replay an active or suspended execution is an error.
- Replay policy may impose limits (e.g., max replay count, replay cooldown period).
- Each replay creates a completely fresh attempt — the worker receives no implicit state from prior attempts unless the execution's input payload includes continuation metadata.
- Replay does NOT reset budget counters (RFC-008). Budget usage is cumulative across all attempts, including prior attempts from before the replay. To replay a budget-constrained execution, the operator must first increase the budget limit or reset usage via `override_budget`.

### Fallback Progression

Fallback occurs when an execution's fallback policy defines a chain of provider/model tiers to try on failure.

**When fallback creates a new attempt:**
Fallback creates a new attempt when the progression semantically restarts model execution with a different provider or model. This is distinct from a simple retry (same provider, same model) because the route and pricing context change.

**Mechanics:**
1. Current attempt ends with `ended_failure` and `failure_category` indicating a provider/model issue (e.g., `provider_error`, `rate_limited_by_provider`, `model_unavailable`).
2. Execution's fallback policy determines the next tier.
3. Engine creates a new attempt with:
   - `attempt_index` = previous + 1
   - `attempt_type` = `fallback`
   - `fallback_index` = next position in the fallback chain
   - `provider_model_snapshot` = the new provider/model target
   - Fresh `AttemptPolicySnapshot` reflecting the new fallback position
4. Execution transitions to `waiting` for a worker with the required capabilities for the new provider/model.

**Fallback chain ownership:** The fallback chain/template belongs to the execution-level policy (ExecutionPolicySnapshot from RFC-001). Each attempt records only the concrete `fallback_index` and `provider_model_snapshot` it used. This separation means the fallback chain can be inspected at execution level while each attempt shows exactly which tier it ran on.

### Per-Attempt Usage and Latency Attribution

Every usage report during an active attempt is recorded against that attempt's `UsageSummary`. The execution-level usage summary is a derived aggregation.

**Attribution rules:**
- `input_tokens`, `output_tokens`, `thinking_tokens`, `total_tokens`, `cost_microdollars` are accumulated per attempt via `report_attempt_usage` operations.
- `latency_ms` is computed as `ended_at - started_at` when the attempt reaches a terminal state.
- `time_to_first_token_ms` is recorded when the first stream frame is appended to this attempt's stream.
- Budget enforcement (RFC-008) charges against the attempt's usage, which rolls up to execution and flow budgets.

**Execution-level derivation:**

| Execution field | Derivation |
|---|---|
| `total_usage` | Sum of all attempts' `UsageSummary` fields. |
| `successful_attempt_usage` | Usage from the attempt that ended with `ended_success`, if any. |
| `wasted_usage` | Sum of usage from non-successful attempts (retries, reclaims, failed fallbacks). |

This decomposition is critical for cost analysis — operators need to know not just "this execution cost $X" but "the successful run cost $Y and failed retries wasted $Z."

### Attempt History as Debugging Timeline

The ordered sequence of attempts for one execution is the **concrete run timeline**. It answers:

- How many times was this tried?
- Why did each attempt end?
- Who owned each attempt?
- Which provider/model did each attempt use?
- How much did each attempt cost?
- How long did each attempt take?
- Was the execution replayed, and from which prior attempt?

**Example timeline:**

```
execution e_abc123:
  attempt[0] initial   → started → ended_failure   (worker-A, gpt-4o, 120ms, provider_timeout)
  attempt[1] fallback  → started → ended_failure   (worker-B, claude-sonnet, 450ms, budget_exceeded)
  attempt[2] retry     → started → ended_success   (worker-C, claude-sonnet, 200ms, ok)
  -- execution completed --
  attempt[3] replay    → started → ended_success   (worker-A, claude-sonnet, 180ms, ok)
  -- execution completed (replay) --
```

### Public API Stance

**Execution-centric primary.** Normal API usage queries executions:
- `get_execution(execution_id)` returns execution state, current attempt index, and a compact attempt summary.
- `list_executions(filter)` returns executions, not attempts.

**Attempt as deep inspection.** Detailed attempt data is available for debugging and operator tools:
- `get_attempt(execution_id, attempt_index)` returns the full attempt record.
- `list_attempts(execution_id)` returns the ordered attempt history.
- `get_current_attempt(execution_id)` returns the latest attempt.

**Execution summary includes attempt context.** The execution summary (RFC-001) should embed:
- `current_attempt_index`
- `current_attempt_state`
- `total_attempt_count`
- `replay_count`

This means most callers never need to query attempts directly, but operators always can.

### AttemptPolicySnapshot

Each attempt captures a frozen policy context at creation time. This answers "what policy was this attempt actually running under?" — distinct from the execution-level policy template.

| Field | Type | Description |
|---|---|---|
| `timeout_ms` | `u64` | Effective timeout for this attempt. May differ from execution default due to fallback-specific overrides or operator adjustments. |
| `route_snapshot` | `RouteSnapshot` | The routing decision made for this attempt: lane, pool, locality, capability match. |
| `provider` | `String` | Provider selected for this attempt (e.g., `openai`, `anthropic`, `bedrock`). |
| `model` | `String` | Model selected for this attempt (e.g., `gpt-4o`, `claude-sonnet-4-5-20250514`). |
| `fallback_index` | `u32` | Position in the execution's fallback chain. `0` = primary. |
| `pricing_snapshot` | `PricingSnapshot` | Token pricing assumptions at attempt creation time. Enables accurate cost attribution even if pricing changes later. |
| `admission_context_summary` | `String` | Human-readable summary of the admission conditions at creation (e.g., "budget 80% consumed, quota 12/100 RPM used"). |

The `RouteSnapshot`:

| Field | Type | Description |
|---|---|---|
| `lane_id` | `String` | Lane this attempt was routed through. |
| `worker_pool` | `String` | Target worker pool, if applicable. |
| `locality` | `String` | Region/zone selection, if applicable. |
| `capability_match` | `Vec<String>` | Capabilities that were matched for routing. |

The `PricingSnapshot`:

| Field | Type | Description |
|---|---|---|
| `input_token_price_microdollars` | `u64` | Price per input token in microdollars. |
| `output_token_price_microdollars` | `u64` | Price per output token in microdollars. |
| `thinking_token_price_microdollars` | `u64` | Price per thinking token in microdollars. |

### Operations

#### create_attempt

Creates a new attempt for an execution. Attempt creation is inline within `ff_claim_execution` (initial/retry/fallback, called by `ff-sdk` via `ff-script`), `ff_reclaim_execution` (reclaim, called by `ff-engine::scanner`), or `ff_replay_execution` (replay, called by `ff-sdk`).

**Parameters:**
- `execution_id: UUID` — target execution
- `attempt_type: AttemptType` — `initial`, `retry`, `reclaim`, `replay`, `fallback`
- `policy_snapshot: AttemptPolicySnapshot` — frozen policy context
- Lineage fields as appropriate for the attempt type

**Semantics:**
- Validates that no other attempt is currently in `started` state for this execution.
- Assigns `attempt_index` = max existing index + 1 (or 0 for initial).
- Generates `attempt_id`.
- Writes attempt record atomically.
- **Atomicity class: A** (must be atomic with execution state transition).

**Errors:**
- `active_attempt_exists` — another attempt is in `started` state.
- `execution_not_found`
- `execution_not_eligible_for_attempt` — execution is in a state that does not permit new attempts (e.g., active and not being reclaimed).
- `replay_not_allowed` — execution is not terminal (for replay type).
- `max_retries_exhausted` — retry policy forbids another attempt.

#### start_attempt

Transitions an attempt from `created` to `started` when a worker acquires a lease.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_id: String`
- `lease_epoch: u64`
- `worker_id: String`
- `worker_instance_id: String`

**Semantics:**
- Validates attempt is in `created` state.
- Records lease binding, worker identity, and `started_at`.
- **Atomicity class: A** (must be atomic with lease acquisition in RFC-003).

**Errors:**
- `attempt_not_found`
- `attempt_not_in_created_state`
- `lease_mismatch`

#### end_attempt

Transitions an attempt to a terminal state. Inline within `ff_complete_execution` or `ff_fail_execution` (called by `ff-sdk` via `ff-script`).

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_id: String`
- `lease_epoch: u64`
- `outcome: AttemptOutcome` — `success`, `failure`, `cancelled`
- `failure_reason: Option<String>`
- `failure_category: Option<String>`
- `usage: Option<UsageSummary>`

**Semantics:**
- Validates attempt is in `started` state and `lease_epoch` matches.
- Records `ended_at`, outcome, and final usage.
- **Atomicity class: A** (must be atomic with execution state transition and lease release in RFC-003).

**Errors:**
- `attempt_not_found`
- `attempt_not_started`
- `stale_lease` — worker's lease_epoch does not match current lease.
- `attempt_already_terminal`

#### suspend_attempt

Transitions an attempt from `started` to `suspended` when the execution intentionally suspends (RFC-004). The attempt is paused, not ended — the same attempt resumes later.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_id: String`
- `lease_epoch: u64`
- `suspension_id: UUID`

**Semantics:**
- Validates attempt is in `started` state and `lease_epoch` matches.
- Sets `attempt_state = suspended` on the attempt hash.
- Records `suspended_at` timestamp on the attempt hash.
- Records `suspension_id` on the attempt hash for correlation.
- Does NOT set `ended_at` (the attempt is not terminal).
- **Atomicity class: A** (must be atomic with execution state transition, lease release, and suspension record creation in RFC-004's `ff_suspend_execution` function).

**Errors:**
- `attempt_not_found`
- `attempt_not_started`
- `stale_lease`

**Note:** This is the operation called by RFC-004's `apply_attempt_pause_for_suspend` helper inside the atomic suspend script.

#### resume_attempt

Transitions an attempt from `suspended` back to `started` when the execution is re-claimed after resume.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_id: String`
- `lease_epoch: u64`

**Semantics:**
- Validates attempt is in `suspended` state.
- Sets `attempt_state = started` on the attempt hash.
- Records new `lease_id` and `lease_epoch` on the attempt hash.
- Clears `suspended_at` and `suspension_id`.
- **Atomicity class: A** (must be atomic with lease acquisition in the re-claim path).

**Errors:**
- `attempt_not_found`
- `attempt_not_suspended`
- `stale_lease`

#### interrupt_attempt

Transitions an attempt to `interrupted_reclaimed`. Called by `ff_reclaim_execution` during reclaim (`ff-engine::scanner`), not by workers.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `reclaim_reason: String`

**Semantics:**
- Validates attempt is in `started` state.
- Records `ended_at` and reclaim reason.
- Does NOT require `lease_epoch` validation (the lease is already expired/revoked).
- **Atomicity class: A** (must be atomic with new attempt creation and new lease issuance).

**Errors:**
- `attempt_not_found`
- `attempt_not_started`

#### report_attempt_usage

Incrementally reports usage against the current active attempt. Workers call `ff-sdk::report_usage()`, which orchestrates the cross-partition sequence via `ff-engine::dispatch`: (1) `ff_report_usage_on_attempt` on `{p:N}` (this operation), then (2) `ff_report_usage_and_check` on each `{b:M}` budget partition (RFC-008).

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_epoch: u64`
- `usage_delta: UsageDelta`

**Semantics:**
- Validates attempt is in `started` state and `lease_epoch` matches.
- Atomically increments usage fields on `{p:N}` (attempt hash + exec core).
- Budget check is a separate `FCALL` on `{b:M}`, orchestrated by `ff-engine::dispatch`.
- **Atomicity class: B** on `{p:N}`. Budget check is cross-partition best-effort (RFC-008 §1.7).

**Errors:**
- `attempt_not_started`
- `stale_lease`
- `budget_exceeded` (if enforcement is active and hard limit is breached)

#### get_attempt

Returns the full attempt record (`ff-sdk` via `ff-script`, single `{p:N}` read).

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`

**Semantics:**
- Read-only. Returns the attempt record or not-found error.
- **Atomicity class: C** (read-only, may be eventually consistent for non-current attempts).

**Errors:**
- `attempt_not_found`

#### list_attempts

Returns the ordered attempt history for an execution.

**Parameters:**
- `execution_id: UUID`
- `offset: Option<u32>` — start from this attempt index
- `limit: Option<u32>` — max results

**Semantics:**
- Returns attempts ordered by `attempt_index` ascending.
- **Atomicity class: C**

**Errors:**
- `execution_not_found`

#### get_current_attempt

Returns the latest attempt for an execution.

**Parameters:**
- `execution_id: UUID`

**Semantics:**
- Returns the attempt with the highest `attempt_index`.
- **Atomicity class: C**

**Errors:**
- `execution_not_found`
- `no_attempts_exist`

### Valkey Data Model

#### Key Schema

All attempt keys use the same `{p:N}` partition hash tag as RFC-003's execution and lease keys. This ensures all keys for one execution — core record, lease, and attempt records — colocate on the same Valkey Cluster shard, enabling atomic operations via Valkey Functions (`FCALL`) across all of them.

The partition tag `p:N` is assigned to each execution at creation time and is stable for the execution's lifetime. `N` is derived from the execution_id (e.g., `crc16(execution_id) % partition_count`).

#### Key Schema

Attempt records are stored using a combination of hashes (per-attempt detail) and a sorted set (per-execution index).

**Attempt index (sorted set):**
```
ff:exec:{p:N}:{execution_id}:attempts  →  ZSET
  member: {attempt_index}
  score:  {attempt_index}   (integer, not timestamp)
```

Scoring by `attempt_index` (integer) guarantees strict monotonic ordering even if two attempts are created within the same millisecond. O(1) access to the latest attempt via `ZREVRANGE ... 0 0`. Range queries by index are natural.

**Attempt detail (hash per attempt):**
```
ff:attempt:{p:N}:{execution_id}:{attempt_index}  →  HASH
  attempt_id          → att_01HYX...
  execution_id        → exec_01HYX...
  attempt_index       → 0
  attempt_type        → initial | retry | reclaim | replay | fallback
  attempt_state       → created | started | ended_success | ...
  created_at          → 1713100800000
  started_at          → 1713100800150
  ended_at            → 1713100801200
  lease_id            → lease_01HYX...
  lease_epoch         → 3
  worker_id           → worker-pool-a-7
  worker_instance_id  → i-0abc123def
  failure_reason      → provider_timeout
  failure_category    → timeout
  fallback_index      → 0
  retry_reason        → (empty)
  reclaim_reason      → (empty)
  replay_reason       → (empty)
  replay_requested_by → (empty)
  replayed_from_idx   → (empty)
  previous_attempt_idx→ (empty)
  terminalized_by_override → 0
```

**Attempt usage (hash per attempt, separate key for hot-path increments):**
```
ff:attempt:{p:N}:{execution_id}:{attempt_index}:usage  →  HASH
  input_tokens            → 1500
  output_tokens           → 350
  thinking_tokens         → 0
  total_tokens            → 1850
  cost_microdollars       → 2775
  latency_ms              → 1050
  ttft_ms                 → 120
  last_usage_report_seq   → 7
```

`last_usage_report_seq` (u64): Monotonically increasing sequence number for idempotent `report_usage` calls. Each `report_usage` call includes a `usage_report_seq` in ARGV. The script checks: if `ARGV.seq <= last_usage_report_seq` → return `ok_already_applied` (no-op, prevents double-counting on retry). Otherwise: HINCRBY counters, HSET `last_usage_report_seq = ARGV.seq`. Starts at 0; the first report uses seq=1.

Separating usage into its own hash avoids contention between usage increment operations (frequent, Class B) and state transitions (less frequent, Class A).

**Attempt policy snapshot (hash per attempt):**
```
ff:attempt:{p:N}:{execution_id}:{attempt_index}:policy  →  HASH
  timeout_ms          → 30000
  provider            → anthropic
  model               → claude-sonnet-4-5-20250514
  fallback_index      → 0
  lane_id             → inference-fast
  worker_pool         → gpu-pool-us-east
  locality            → us-east-1
  input_price_usd6    → 3000
  output_price_usd6   → 15000
  thinking_price_usd6 → 3000
  admission_summary   → budget 45% used, quota 8/100 RPM
```

**Current attempt pointer (on execution core hash, RFC-001/RFC-003):**
```
ff:exec:{p:N}:{execution_id}:core  →  HASH
  ...
  current_attempt_index → 2
  current_attempt_state → started
  total_attempt_count   → 3
  replay_count          → 0
  ...
```

This denormalization on the execution core hash avoids a sorted-set lookup for the most common query pattern ("what is the current attempt?").

#### Atomic Operations (Valkey Functions)

All operations below are registered as Valkey Functions within the `flowfabric` library (see RFC-010 §4.8). They operate on keys sharing the `{p:N}` hash tag, ensuring single-shard atomicity in Valkey Cluster. Invoked via `FCALL <function_name> <numkeys> <keys...> <args...>`.

**ff_claim_execution (claim path):**
For the common case where claim + attempt creation + lease acquisition happen together. Registered as `ff_claim_execution` in the `flowfabric` library.

```
-- KEYS (all share {p:N} hash tag):
--   exec_core, attempts_zset, attempt_hash, attempt_usage, attempt_policy,
--   lease_current, lease_history, claim_grant
-- ARGV: attempt fields, lease fields, worker_id, worker_instance_id,
--        capability_snapshot, timestamp
-- Atomically:
--   1. Verify no attempt is in 'started' state (check exec core current_attempt_state)
--   2. Validate claim_grant matches worker_id, worker_instance_id, capability snapshot
--   3. Delete consumed claim_grant
--   4. Compute next attempt_index from exec core current_attempt_index + 1
--   5. Generate attempt_id
--   6. Write attempt hash with attempt_state = started, started_at = now
--   7. ZADD attempts_zset with score = attempt_index, member = attempt_index
--   8. Write attempt policy snapshot hash
--   9. Initialize attempt usage hash (all counters = 0)
--  10. Increment lease_epoch on exec core
--  11. Update exec core: current_attempt_index, current_attempt_state = running_attempt,
--      lifecycle_phase = active, ownership_state = leased, current_lease_id, etc.
--  12. Write lease_current hash (RFC-003)
--  13. ZADD partition lease_expiry index
--  14. XADD lease_history
-- Returns: attempt_id, attempt_index, lease_epoch, lease_expires_at
```

**ff_complete_execution / ff_fail_execution (completion/failure path):**
Registered as `ff_complete_execution` and `ff_fail_execution` in the `flowfabric` library. See RFC-001 for full pseudocode.
```
-- KEYS (all share {p:N} hash tag):
--   exec_core, attempt_hash, attempt_usage, lease_current, lease_history,
--   lease_expiry_idx, worker_leases, stream_meta
-- ARGV: outcome, attempt_index, lease_id, lease_epoch, final_usage, timestamp
-- Atomically:
--   1. Verify lease_id and lease_epoch match exec core current lease
--   2. Verify current_attempt_index matches ARGV attempt_index
--   3. Verify attempt is in 'started' state
--   4. Set attempt_state = ended_success|ended_failure|ended_cancelled, ended_at
--   5. Merge final usage into attempt usage hash
--   6. Update exec core: current_attempt_state, lifecycle_phase, terminal_outcome,
--      ownership_state = unowned, clear lease fields
--   7. Release lease: DEL lease_current, ZREM lease_expiry, SREM worker_leases
--   8. XADD lease_history (released event)
--   9. Close attempt stream: if stream_meta exists, set closed_at and
--      closed_reason = attempt_success|attempt_failure|attempt_cancelled (RFC-006)
--  10. If retry policy applies: return retry_needed = true (caller creates next attempt)
-- Returns: attempt_id, retry_needed, next_delay_ms
```

**ff_reclaim_execution (interrupt and reclaim):**
Registered as `ff_reclaim_execution` in the `flowfabric` library. See RFC-003 for full pseudocode.
```
-- KEYS (all share {p:N} hash tag):
--   exec_core, old_attempt_hash, new_attempt_hash, attempts_zset,
--   new_policy, new_usage, old_lease, new_lease, lease_history,
--   lease_expiry_idx, old_worker_leases, new_worker_leases, claim_grant,
--   old_stream_meta
-- ARGV: reclaim_reason, new_attempt_fields, new_worker_id, new_worker_instance_id,
--        timestamp
-- Atomically:
--   1. Validate claim_grant for new worker
--   2. Set old attempt state to interrupted_reclaimed, ended_at
--   3. Invalidate old lease: DEL old_lease, SREM old_worker_leases
--   4. XADD lease_history (expired + reclaimed events)
--   5. Close old attempt stream: if old_stream_meta exists, set closed_at and
--      closed_reason = attempt_interrupted (RFC-006)
--   6. Compute new attempt_index = current + 1
--   7. Create new attempt hash with attempt_state = started, attempt_type = reclaim
--   8. ZADD attempts_zset with score = new_attempt_index
--   9. Write new policy/usage hashes
--  10. Increment lease_epoch, create new lease
--  11. Update exec core: current_attempt_index, current_attempt_state = running_attempt,
--      ownership_state = leased, new lease fields, reclaim_count++
--  12. ZADD lease_expiry with new expires_at
--  13. SADD new_worker_leases
--  14. Delete consumed claim_grant
-- Returns: new_attempt_id, new_attempt_index, new_lease_epoch
```

#### Retention and Cleanup

Attempt records follow the execution's retention policy. Purged by the terminal retention scanner (`ff-engine::scanner`) as part of the execution cleanup cascade (RFC-010 §9.3 steps 2-3):
1. Delete `ff:attempt:{p:N}:{execution_id}:*` (all attempt hashes, usage hashes, policy hashes).
2. Delete `ff:exec:{p:N}:{execution_id}:attempts` (the sorted set).

For executions with very long attempt histories (e.g., many replays over weeks), the engine may optionally compact old attempt records into a summary, retaining only the latest N attempt details and an aggregate summary of older ones.

### Error Model

| Error | When |
|---|---|
| `attempt_not_found` | Requested attempt index does not exist. |
| `active_attempt_exists` | Tried to create a new attempt but one is already in `started` state. |
| `attempt_not_in_created_state` | Tried to start an attempt that is not in `created` state. |
| `attempt_not_started` | Tried to end/report-usage on an attempt that is not in `started` state. |
| `attempt_already_terminal` | Tried to transition an already-terminal attempt. |
| `stale_lease` | Worker's `lease_epoch` does not match the current lease — stale owner. |
| `execution_not_found` | Parent execution does not exist. |
| `execution_not_eligible_for_attempt` | Execution state does not allow a new attempt (e.g., already active). |
| `replay_not_allowed` | Tried to replay a non-terminal execution, or replay limit exceeded. |
| `max_retries_exhausted` | Retry policy forbids another attempt. |

## Interactions with Other Primitives

### RFC-001 — Execution

The execution object (RFC-001) is the parent of all attempts. Key interactions:

- Execution carries `current_attempt_index`, `current_attempt_state`, `total_attempt_count`, and `replay_count` as denormalized fields for fast access.
- Execution lifecycle transitions are driven by attempt outcomes: `ended_success` → execution `completed`, `ended_failure` with no retry → execution `failed`, etc.
- The execution's **attempt_state** dimension in the orthogonal state vector (RFC-001 Section 2.2F) reflects the current attempt's state: `pending_first_attempt`, `running_attempt`, `attempt_interrupted`, `pending_retry_attempt`, `pending_replay_attempt`, `attempt_terminal`.
- ExecutionPolicySnapshot (execution-level) owns the retry policy template and fallback chain. AttemptPolicySnapshot (this RFC) captures the concrete policy each attempt ran with.

### RFC-003 — Lease

The lease (RFC-003) binds to the current active attempt. The primary binding key is `attempt_index` (monotonic, used in Valkey keys); `attempt_id` is recorded for audit correlation only. Key interactions:

- `lease_id` and `lease_epoch` are recorded on the attempt when it transitions to `started`.
- Lease acquisition is atomic with attempt start (`ff_claim_execution` function).
- Lease release is atomic with attempt end (`ff_complete_execution` / `ff_fail_execution` functions).
- Lease expiry triggers attempt interruption + reclaim (`ff_reclaim_execution` function).
- `lease_epoch` is monotonic per execution. Reclaim always increments it, ensuring stale-owner rejection.
- Lease renewal does NOT create a new attempt — it extends the current attempt's lease.

### RFC-006 — Stream

Streams are attempt-scoped. Each attempt may have its own ordered output stream. The stream is identified by `(execution_id, attempt_index)`. A merged execution-level stream view aggregates across attempts. Per-attempt stream attribution depends on attempt identity being stable and ordered (invariants A1, A2).

## V1 Scope

### V1 must-have

- Full attempt object with all identity, timing, outcome, ownership, and lineage fields.
- All 7 lifecycle states and their transitions (including `suspended`).
- Invariants A1-A5 enforced.
- Retry creates a new attempt with incremented index.
- Reclaim creates a new attempt with new `lease_epoch`.
- Replay creates a new attempt on terminal executions.
- Fallback progression creates a new attempt.
- Per-attempt usage attribution (tokens, cost, latency).
- AttemptPolicySnapshot with route, provider/model, fallback index, timeout, and pricing.
- Valkey data model: attempt hash, sorted set index, usage hash, policy hash.
- Atomic Valkey Functions for create+start, end+decide, interrupt+reclaim (registered in `flowfabric` library).
- `get_attempt`, `list_attempts`, `get_current_attempt` APIs.
- Execution-level denormalized attempt summary fields.

### Designed-for but deferred

- Attempt history compaction for long-lived executions.
- Attempt-level metrics aggregation pipelines.
- Cross-attempt diff views (e.g., "what changed between attempt 2 and attempt 3?").
- Attempt-level access control (v1 inherits from execution).
- Attempt archival to external storage.

## Open Questions

1. **Compaction threshold:** At what attempt count should the engine begin compacting old attempt records into summaries? This is a product/operational decision, not a correctness concern.

**Resolved:** Attempt record TTL = inherited from parent execution. Confirmed by RFC-010 §9.3 retention defaults.

**Resolved:** Fallback within same tier = `attempt_type = retry`. The fallback_index does not change, so it is a retry within the same fallback position.

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 1.5, Section 3.4, Finalization items 3 and 4
- RFC-001: Execution Object and State Model (worker-2)
- RFC-003: Lease and Fencing Semantics (worker-1)
- RFC-006: Stream Model (worker-3)
