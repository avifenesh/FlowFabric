# RFC-001: Execution Object and Orthogonal State Model

**Status:** Draft
**Author:** Worker-2 (Client & Public API Specialist)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 1, Sections 2.0–2.9, Finalization §2–§4

---

## Summary

This RFC defines the **Execution** object — the primary unit of controlled work in FlowFabric — and the **orthogonal state model** that governs it. Execution state is a constrained 6-dimension vector (lifecycle phase, ownership state, eligibility state, blocking reason, terminal outcome, attempt state) from which the engine deterministically derives a stable `public_state` label. This RFC also defines the full field schema, all execution operations grouped by class, the validity constraint matrix, state transition rules, the Valkey data model, and the execution result/lineage/visibility models. Everything else in FlowFabric — lease, suspension, signal, stream, flow, budget — attaches to execution.

## Motivation

FlowFabric is an execution engine, not just a queue. A queue can get away with a flat state enum (`waiting → active → done`). FlowFabric cannot, because it must simultaneously answer:

- What phase is this execution in? (lifecycle)
- Who owns it right now? (ownership/lease)
- Can it run now? (eligibility/scheduling)
- If not, why not? (blocking reason)
- If it ended, how? (terminal outcome)
- What is the current attempt doing? (attempt state)

A single enum for all of this produces state explosion and poor invariants. The orthogonal state vector solves this cleanly.

**Driving use cases:** UC-01 through UC-10 (baseline execution), UC-11 through UC-18 (controlled lifecycle), UC-19 through UC-26 (interruptible execution), UC-27 through UC-35 (flow coordination), UC-36 through UC-48 (AI-native execution), UC-55 through UC-62 (control surface).

---

## Detailed Design

### 1. Execution Object Definition

An execution is a **durable, inspectable, controllable unit of work** with identity, lifecycle state, input payload, execution policy, ownership information, optional flow membership, optional suspension state, optional output stream, usage/budget accounting, retry/failure history, and control surface visibility.

Execution is **not**: the user-facing product workflow object, the long-term business source of truth, the prompt or memory object, the queue itself, or the worker process.

#### 1.1 Core Invariants

| ID | Invariant | Rationale |
|----|-----------|-----------|
| E1 | **Stable identity.** Each execution has a stable `execution_id` for its entire lifetime. Retries, suspensions, resumes, reclaims, replays, and operator actions do not create a new logical execution. | Lineage, audit, and correlation all depend on one stable ID. |
| E2 | **Explicit lifecycle state.** At any moment, an execution has one authoritative state vector. State transitions are explicit, auditable, and driven by engine operations. | Prevents ghost states and implicit transitions. |
| E3 | **Single active owner.** An execution may have at most one active lease owner at a time. Concurrent completion or conflicting mutations must be rejected atomically. | Prevents split-brain completion. See RFC-003. |
| E4 | **Durable control.** Suspend, signal, resume, cancel, fail, retry, and complete are durable state transitions, not best-effort hints. | Crash recovery depends on durable state. |
| E5 | **Inspectability.** It must always be possible to inspect the execution's current state, key timestamps, retry status, routing intent, and blocking reason. | "Why is this stuck?" must always be answerable. |
| E6 | **Composability.** Execution is usable standalone, inside a queue facade, and inside a flow/DAG without changing its identity or semantics. | One primitive, many usage patterns. |
| E7 | **Resource attachment.** Usage, budgets, quotas, and fallbacks attach to execution and execution groups, not only to worker processes. | Enables flow-scoped budget enforcement. |
| E8 | **Input immutability.** The `input_payload` is set once on creation and never modified. Retries, reclaims, replays, and fallback progressions all re-execute with the same input. To run with different input, create a new execution. | Stable input guarantees reproducibility and simplifies replay semantics. |
| E9 | **Result overwrite on replay.** The `result_payload` is set atomically on completion and overwritten if the execution is replayed and completes again. Only the latest terminal result is retained at the execution level. Per-attempt results are not stored — archive externally before replay if the old result must be preserved. | Execution result is "the current answer," not a history of answers. Attempt records provide usage/timing lineage but not result data. |

#### 1.2 Execution Fields

##### Identity

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `execution_id` | `UUID` | yes | Stable identity for the logical execution. Never changes across retries/reclaims/replays. |
| `partition_id` | `u32` | yes | Valkey partition assignment: `hash(execution_id) % num_partitions`. Computed once at creation, immutable. |
| `namespace` | `String` | yes | Tenant or workspace scope. |
| `lane_id` | `String` | yes | Submission lane (queue-compatible ingress). |
| `idempotency_key` | `String` | no | Caller-supplied dedup key. If present, engine rejects duplicate submissions with same key within the dedup window. |
| `tags` | `Map<String, String>` | no | User-supplied labels for filtering and search. |

##### Payload

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `execution_kind` | `String` | yes | Execution type name (e.g. `"llm_call"`, `"etl_step"`). Used for routing and metrics. |
| `input_payload` | `Bytes` | yes | Opaque input data. |
| `payload_encoding` | `String` | no | Encoding hint (e.g. `"json"`, `"protobuf"`, `"msgpack"`). Default: `"json"`. |

##### Policy (ExecutionPolicySnapshot)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `priority` | `i32` | no | Higher value = higher priority. Default: `0`. |
| `delay_until` | `Timestamp` | no | Earliest eligible time. If set and future, execution starts as `delayed`. |
| `retry_policy` | `RetryPolicy` | no | Max attempts, backoff strategy, retryable error classes. |
| `timeout_policy` | `TimeoutPolicy` | no | Per-attempt timeout, total execution deadline, max reclaim count. |
| `max_reclaim_count` | `u32` | no | Maximum lease-expiry reclaims before the execution is failed with `max_reclaims_exceeded`. Default: `100`. Prevents unbounded attempt creation from flapping workers. Enforced by `reclaim_execution`. |
| `suspension_policy` | `SuspensionPolicy` | no | Default suspension timeout behavior. |
| `fallback_policy` | `FallbackPolicy` | no | Fallback chain template (provider/model progression). |
| `max_replay_count` | `u32` | no | Maximum number of replays allowed. Default: `10`. Enforced by `replay_execution`. |
| `budget_ids` | `Vec<UUID>` | no | Attached budget references. |
| `routing_requirements` | `RoutingRequirements` | no | Required capabilities, preferred locality, isolation level. |
| `dedup_window` | `Duration` | no | Window for idempotency_key dedup. V1 default: 24h. Actual TTL = `min(dedup_window, retention_window)` to prevent stale key outliving execution. |
| `stream_policy` | `StreamPolicy` | no | Durability mode for attempt streams. |
| `max_signals_per_execution` | `u32` | no | Maximum signal records accepted for this execution. Default: `10000`. Prevents unbounded memory growth from webhook retry storms. Enforced in `deliver_signal` Lua script. |

##### Runtime State (the state vector — see §2)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `lifecycle_phase` | `LifecyclePhase` | yes | Structural phase dimension. |
| `ownership_state` | `OwnershipState` | yes | Who may mutate. |
| `eligibility_state` | `EligibilityState` | yes | Can it run now. |
| `blocking_reason` | `BlockingReason` | yes | Why not progressing. |
| `blocking_detail` | `String` | no | Extended human-readable explanation when `blocking_reason` is set (e.g., `"budget budget-abc123: total_cost 48M/50M"`). Cleared when `blocking_reason = none`. |
| `terminal_outcome` | `TerminalOutcome` | yes | How it ended (if terminal). |
| `attempt_state` | `AttemptState` | yes | Current attempt layer state. |
| `public_state` | `PublicState` | yes | Engine-derived user-facing label. |
| `current_attempt_index` | `u32` | yes | Current attempt number (0-based, monotonically increasing). See RFC-002. |
| `total_attempt_count` | `u32` | yes | Total number of attempts created for this execution. |
| `created_at` | `Timestamp` | yes | When the execution was created. |
| `started_at` | `Timestamp` | no | When first claimed. |
| `completed_at` | `Timestamp` | no | When terminally resolved. |
| `last_transition_at` | `Timestamp` | yes | Last state vector change. |
| `failure_reason` | `String` | no | Structured reason if failed. |
| `cancellation_reason` | `String` | no | Structured reason if cancelled. |

##### Ownership References (authoritative field list per RFC-003)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `current_lease_id` | `UUID` | no | Current lease if active. See RFC-003. |
| `current_lease_epoch` | `u64` | no | Monotonic fencing token. Increments on each new lease issuance. |
| `current_attempt_id` | `UUID` | no | The attempt bound to the current lease. |
| `current_worker_id` | `String` | no | Logical worker identity. |
| `current_worker_instance_id` | `String` | no | Concrete worker process/runtime instance identity. |
| `current_lane` | `String` | no | Lane used at lease acquisition time. |
| `lease_acquired_at` | `Timestamp` | no | When current lease was issued. |
| `lease_expires_at` | `Timestamp` | no | Hard ownership deadline. |
| `lease_last_renewed_at` | `Timestamp` | no | Last successful renewal. |
| `lease_renewal_deadline` | `Timestamp` | no | Recommended latest time to renew before expiration. |
| `lease_reclaim_count` | `u32` | no | Count of reclaim-driven ownership turnovers. |
| `lease_revoked_at` | `Timestamp` | no | Set when lease was intentionally revoked. |
| `lease_revoke_reason` | `String` | no | Operator or system reason for revocation. |
| `lease_expired_at` | `Timestamp` | no | Set when lease expiry is detected by scanner or mutation path. |
| `current_suspension_id` | `UUID` | no | Current suspension episode if suspended. Cleared on resume/cancel/timeout. |
| `current_waitpoint_id` | `UUID` | no | Current waitpoint if suspended. Cleared on resume/cancel/timeout. |

##### Relationships

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `parent_execution_id` | `UUID` | no | Parent execution if in a flow tree. |
| `flow_id` | `UUID` | no | Flow membership if any. |
| `dependency_ids` | `Vec<UUID>` | no | Upstream execution dependencies. |
| `child_count` | `u32` | no | Number of child executions spawned. |
| `completed_child_count` | `u32` | no | Children that reached terminal success. |
| `failed_child_count` | `u32` | no | Children that reached terminal failure. |

##### Output and Result

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `result_payload` | `Bytes` | no | Final result data (set on completion). |
| `result_encoding` | `String` | no | Encoding of result payload. |
| `progress_pct` | `u8` | no | Last reported progress (0–100). |
| `progress_message` | `String` | no | Last reported progress description. |

##### Accounting

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `total_input_tokens` | `u64` | no | Cumulative input tokens across attempts. |
| `total_output_tokens` | `u64` | no | Cumulative output tokens. |
| `total_thinking_tokens` | `u64` | no | Cumulative thinking/reasoning tokens. |
| `total_cost_micros` | `u64` | no | Cumulative cost in microcurrency units. |
| `total_latency_ms` | `u64` | no | Cumulative wall-clock processing time. |
| `retry_count` | `u32` | no | Number of retry attempts. |
| `reclaim_count` | `u32` | no | Number of reclaim events. |
| `replay_count` | `u32` | no | Number of replay events. |
| `fallback_index` | `u32` | no | Current position in fallback chain. |

##### Audit

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `creator_identity` | `String` | yes | Who/what created the execution. |
| `last_mutation_at` | `Timestamp` | yes | Last write to any field. |
| `last_operator_action` | `String` | no | Description of last privileged override. |
| `last_operator_action_at` | `Timestamp` | no | When last privileged override occurred. |

---

### 2. Orthogonal State Model

#### 2.1 Why Orthogonal State

Execution state is not a single giant enum. Different dimensions answer different questions. The engine needs to simultaneously track phase, ownership, eligibility, blocking reason, outcome, and attempt progress. Forcing all of this into one enum causes state explosion (`active_leased_eligible` vs `active_leased_reclaimable` vs `active_revoked_...` × every blocking reason × every outcome = hundreds of variants).

The orthogonal model keeps each concern in its own dimension with a small number of values and enforces validity constraints on combinations.

#### 2.2 State Vector Dimensions

```
┌─────────────────────────────────────────────────────────────────┐
│                    EXECUTION STATE VECTOR                        │
│                                                                  │
│  A. lifecycle_phase    ── what major phase of existence?         │
│  B. ownership_state    ── who may mutate active state?           │
│  C. eligibility_state  ── can it be claimed for work now?        │
│  D. blocking_reason    ── why is it not progressing?             │
│  E. terminal_outcome   ── how did it end?                        │
│  F. attempt_state      ── what is the attempt layer doing?       │
│                                                                  │
│  Derived: public_state ── engine-computed user-facing label      │
└─────────────────────────────────────────────────────────────────┘
```

##### Dimension A — Lifecycle Phase

The broadest anchored dimension. Answers: *what major phase of existence is this execution in?*

| Value | Description |
|-------|-------------|
| `submitted` | Accepted by engine, not yet resolved to runnable/delayed. Transient. |
| `runnable` | Eligible or potentially eligible for claiming. May be delayed, blocked, or ready. |
| `active` | Currently owned by a worker lease and in progress. |
| `suspended` | Intentionally paused, waiting for signal/approval/callback. No active lease. |
| `terminal` | Execution is finished. No further state transitions except replay. |

##### Dimension B — Ownership State

Answers: *who, if anyone, is currently allowed to mutate active execution state?*

| Value | Description |
|-------|-------------|
| `unowned` | No current lease. Normal for runnable, suspended, terminal. |
| `leased` | A worker holds a valid lease. Normal for active. |
| `lease_expired_reclaimable` | Lease TTL passed without renewal. Execution awaits reclaim. |
| `lease_revoked` | Lease was explicitly revoked by operator or engine. |

##### Dimension C — Eligibility State

Answers: *can the execution be claimed for work right now?*

| Value | Description |
|-------|-------------|
| `eligible_now` | Ready for claiming. |
| `not_eligible_until_time` | Delayed until a future timestamp (backoff, schedule, explicit delay). |
| `blocked_by_dependencies` | Waiting on upstream executions in a flow/DAG. |
| `blocked_by_budget` | Budget limit reached. |
| `blocked_by_quota` | Quota or rate-limit reached. |
| `blocked_by_route` | No capable/available worker matches requirements. |
| `blocked_by_lane_state` | Lane is paused or draining. |
| `blocked_by_operator` | Operator hold. |
| `not_applicable` | Used when lifecycle_phase is active, suspended, or terminal. |

##### Dimension D — Blocking Reason

Answers: *what is the most specific explanation for lack of forward progress?* This is explanatory/human-facing. It does not replace the structural dimensions.

| Value | Description |
|-------|-------------|
| `none` | Not blocked (active, terminal, or truly ready). |
| `waiting_for_worker` | Eligible but no worker has claimed yet. |
| `waiting_for_retry_backoff` | Delayed for retry backoff. |
| `waiting_for_resume_delay` | Delayed after suspension resume (resume_delay_ms). |
| `waiting_for_delay` | Worker-initiated explicit delay (via `delay_execution`). |
| `waiting_for_signal` | Suspended, waiting for a generic signal. |
| `waiting_for_approval` | Suspended, waiting for human approval. |
| `waiting_for_callback` | Suspended, waiting for external callback. |
| `waiting_for_tool_result` | Suspended, waiting for tool completion. |
| `waiting_for_children` | Blocked on child/dependency executions. |
| `waiting_for_budget` | Budget exhausted. |
| `waiting_for_quota` | Quota/rate-limit window full. |
| `waiting_for_capable_worker` | No worker with required capabilities available. |
| `waiting_for_locality_match` | No worker in required region/locality. |
| `paused_by_operator` | Operator placed a hold. |
| `paused_by_policy` | Policy rule (e.g. lane pause) prevents progress. |
| `paused_by_flow_cancel` | Flow cancellation with `let_active_finish` policy blocked this unclaimed member. Only `cancel_flow` clears this — the unblock scanner must skip it. |

##### Dimension E — Terminal Outcome

Answers: *if the execution is terminal, how did it end?*

| Value | Description |
|-------|-------------|
| `none` | Not terminal. |
| `success` | Completed successfully with result. |
| `failed` | Failed after exhausting retries or by explicit failure. |
| `cancelled` | Intentionally terminated by user, operator, or policy. |
| `expired` | Deadline, TTL, or suspension timeout elapsed. |
| `skipped` | Required dependency failed, making this execution impossible. Required for DAG correctness. |

##### Dimension F — Attempt State

Answers: *what is happening at the concrete run-attempt layer?* See RFC-002 for full attempt model.

| Value | Description |
|-------|-------------|
| `none` | No attempt context (e.g. freshly submitted). |
| `pending_first_attempt` | Awaiting initial claim. |
| `running_attempt` | An attempt is actively executing. |
| `attempt_interrupted` | Current attempt was interrupted (crash, reclaim, suspension). |
| `pending_retry_attempt` | Awaiting retry after failure. |
| `pending_replay_attempt` | Awaiting replay after terminal state. |
| `attempt_terminal` | The final attempt has concluded. |

#### 2.3 Public State Taxonomy

Even though the engine stores a 6-dimension vector, users get a single `public_state` label. The engine derives it centrally. **Clients must not invent their own mapping.**

| Public State | Meaning |
|--------------|---------|
| `waiting` | Eligible and waiting for a worker to claim. |
| `delayed` | Not yet eligible due to time-based delay (schedule, backoff). |
| `rate_limited` | Blocked by budget, quota, or rate-limit policy. |
| `waiting_children` | Blocked on child/dependency executions. |
| `active` | Currently being processed by a worker. |
| `suspended` | Intentionally paused, waiting for signal/approval/callback. |
| `completed` | Terminal: finished successfully. |
| `failed` | Terminal: finished unsuccessfully. |
| `cancelled` | Terminal: intentionally terminated. |
| `expired` | Terminal: deadline/TTL elapsed. |
| `skipped` | Terminal: impossible to run because required dependency failed. |

#### 2.4 Deterministic Derivation Rules

The engine derives `public_state` from the state vector using these principles, applied in priority order:

##### Principle D1 — Terminal outcome dominates

If `lifecycle_phase = terminal`, `public_state` is determined solely by `terminal_outcome`:

```
terminal_outcome = success   → public_state = completed
terminal_outcome = failed    → public_state = failed
terminal_outcome = cancelled → public_state = cancelled
terminal_outcome = expired   → public_state = expired
terminal_outcome = skipped   → public_state = skipped
```

##### Principle D2 — Suspended is explicit

If `lifecycle_phase = suspended`, `public_state = suspended`.

Suspension is intentional and durable. It must not be hidden as "just another blocked runnable."

##### Principle D3 — Active dominates non-terminal ownership nuance

If `lifecycle_phase = active`, `public_state = active`.

Even if `ownership_state = lease_expired_reclaimable` or `lease_revoked`, the public label remains `active`. The deeper ownership field explains the nuance for operators who drill down.

##### Principle D4 — Runnable states render by eligibility and blocking reason

If `lifecycle_phase = runnable`:

```
eligibility_state = eligible_now
    → public_state = waiting

eligibility_state = not_eligible_until_time
    → public_state = delayed

eligibility_state = blocked_by_dependencies
    AND blocking_reason = waiting_for_children
    → public_state = waiting_children

eligibility_state = blocked_by_budget
    OR eligibility_state = blocked_by_quota
    → public_state = rate_limited

eligibility_state = blocked_by_route
    → public_state = waiting
    (blocking_reason carries placement-specific explanation)

eligibility_state = blocked_by_lane_state
    → public_state = waiting
    (blocking_reason = paused_by_policy)

eligibility_state = blocked_by_operator
    → public_state = waiting
    (blocking_reason = paused_by_operator)
```

##### Principle D5 — Submitted collapses to waiting

If `lifecycle_phase = submitted`, `public_state = waiting`. The `submitted` phase is transient and not externally distinguished.

##### Derivation pseudocode

```rust
fn derive_public_state(sv: &StateVector) -> PublicState {
    match sv.lifecycle_phase {
        Terminal => match sv.terminal_outcome {
            Success   => Completed,
            Failed    => Failed,
            Cancelled => Cancelled,
            Expired   => Expired,
            Skipped   => Skipped,
            None      => unreachable!(), // validity constraint
        },
        Suspended => Suspended,
        Active    => Active,
        Runnable  => match sv.eligibility_state {
            EligibleNow              => Waiting,
            NotEligibleUntilTime     => Delayed,
            BlockedByDependencies    => WaitingChildren,
            BlockedByBudget
            | BlockedByQuota         => RateLimited,
            BlockedByRoute
            | BlockedByLaneState
            | BlockedByOperator      => Waiting,
            NotApplicable            => unreachable!(),
        },
        Submitted => Waiting,
    }
}
```

#### 2.5 Example State Vectors

##### Ready and waiting for a worker

```
lifecycle_phase   = runnable
ownership_state   = unowned
eligibility_state = eligible_now
blocking_reason   = waiting_for_worker
terminal_outcome  = none
attempt_state     = pending_first_attempt
─────────────────────────────────────────
public_state      = waiting
```

##### Delayed retry backoff

```
lifecycle_phase   = runnable
ownership_state   = unowned
eligibility_state = not_eligible_until_time
blocking_reason   = waiting_for_retry_backoff
terminal_outcome  = none
attempt_state     = pending_retry_attempt
─────────────────────────────────────────
public_state      = delayed
```

##### Waiting on children (DAG/flow, never claimed)

```
lifecycle_phase   = runnable
ownership_state   = unowned
eligibility_state = blocked_by_dependencies
blocking_reason   = waiting_for_children
terminal_outcome  = none
attempt_state     = pending_first_attempt
─────────────────────────────────────────
public_state      = waiting_children
```

##### Actively running with valid lease

```
lifecycle_phase   = active
ownership_state   = leased
eligibility_state = not_applicable
blocking_reason   = none
terminal_outcome  = none
attempt_state     = running_attempt
─────────────────────────────────────────
public_state      = active
```

##### Worker crashed, execution reclaimable

```
lifecycle_phase   = active
ownership_state   = lease_expired_reclaimable
eligibility_state = not_applicable
blocking_reason   = none
terminal_outcome  = none
attempt_state     = attempt_interrupted
─────────────────────────────────────────
public_state      = active  (D3: active dominates ownership nuance)
```

##### Lease revoked by operator, awaiting reclaim

```
lifecycle_phase   = active
ownership_state   = lease_revoked
eligibility_state = not_applicable
blocking_reason   = none
terminal_outcome  = none
attempt_state     = attempt_interrupted
─────────────────────────────────────────
public_state      = active  (D3: active dominates ownership nuance)
```

##### Suspended for approval

```
lifecycle_phase   = suspended
ownership_state   = unowned
eligibility_state = not_applicable
blocking_reason   = waiting_for_approval
terminal_outcome  = none
attempt_state     = attempt_interrupted
─────────────────────────────────────────
public_state      = suspended
```

##### Terminal success

```
lifecycle_phase   = terminal
ownership_state   = unowned
eligibility_state = not_applicable
blocking_reason   = none
terminal_outcome  = success
attempt_state     = attempt_terminal
─────────────────────────────────────────
public_state      = completed
```

##### Skipped (DAG dependency failed)

```
lifecycle_phase   = terminal
ownership_state   = unowned
eligibility_state = not_applicable
blocking_reason   = none
terminal_outcome  = skipped
attempt_state     = none
─────────────────────────────────────────
public_state      = skipped
```

#### 2.6 Validity Constraint Matrix

Orthogonal dimensions do **not** mean all combinations are legal. The engine must enforce these constraints atomically on every state transition.

##### Hard constraints (must never occur)

| # | Constraint | Rationale |
|---|-----------|-----------|
| V1 | `lifecycle_phase = terminal` AND `ownership_state = leased` | Terminal executions must not retain active leases. |
| V2 | `lifecycle_phase = suspended` AND `ownership_state = leased` | Suspension releases ownership (Invariant S2 from pre-RFC). |
| V3 | `terminal_outcome != none` AND `lifecycle_phase != terminal` | Non-terminal executions cannot have a terminal outcome. |
| V4 | `lifecycle_phase = terminal` AND `terminal_outcome = none` | Terminal executions must have an outcome. |
| V5 | `lifecycle_phase = active` AND `attempt_state = pending_first_attempt` | If active, an attempt must be running. |
| V6 | `eligibility_state = eligible_now` AND `lifecycle_phase = terminal` | Terminal executions are not eligible. |
| V7 | `eligibility_state = eligible_now` AND `lifecycle_phase = active` | Already claimed — not eligible for re-claim. |
| V8 | `lifecycle_phase = submitted` AND `ownership_state = leased` | Cannot be leased before reaching runnable. |
| V9 | `lifecycle_phase = runnable` AND `attempt_state = running_attempt` | If an attempt is running, lifecycle must be active. |
| V10 | `lifecycle_phase = active` AND `ownership_state = unowned` | Active requires some ownership state (leased or reclaimable). |

##### Valid but subtle combinations (expected in production)

| Combination | Explanation |
|-------------|-------------|
| `active` + `lease_expired_reclaimable` | Worker stopped heartbeating. Execution is technically still "active" until reclaimed. |
| `active` + `lease_revoked` | Operator or engine revoked the lease. Execution remains `active` in an inspectable non-trusted ownership state until reclaim or override resolves it. The revoked state is observable, not instantly cleared. |
| `runnable` + `waiting_for_children` | Execution entered `runnable` but dependencies block claiming. |
| `runnable` + `not_eligible_until_time` | Retry backoff or explicit delay. |
| `suspended` + `attempt_interrupted` | The running attempt was interrupted by the suspension. |
| `terminal` + `attempt_state = none` | Execution was `skipped` before any attempt was created. |
| `runnable` + `eligible_now` + `attempt_interrupted` | Worker called `move_to_waiting_children` (attempt paused), all deps resolved via `resolve_dependency`. Execution is eligible for re-claim via `claim_resumed_execution` (same attempt continues). Also occurs after `delay_execution` when the delay expires and the delayed promoter runs. |
| `runnable` + `blocked_by_dependencies` + `attempt_interrupted` | Worker called `move_to_waiting_children`. Attempt is paused, awaiting upstream deps. When all deps resolve, transitions to `eligible_now` + `attempt_interrupted` (above). |
| `runnable` + `not_eligible_until_time` + `attempt_interrupted` | Worker called `delay_execution`. Attempt is paused during the backoff delay. When delay expires, promoter sets `eligible_now` (preserving `attempt_interrupted`). |
| `runnable` + `eligible_now` + `pending_replay_attempt` | Operator called `replay_execution` on a non-skipped terminal execution. New replay attempt is pending claim via `claim_execution`. |
| `runnable` + `blocked_by_dependencies` + `pending_replay_attempt` | Operator called `replay_execution` on a `skipped` flow member. Dep edges reset to unsatisfied, awaiting re-resolution before the replay attempt can be claimed. |

---

### 3. State Transition Rules

#### 3.1 Allowed Transitions (lifecycle_phase)

```
                    ┌──────────┐
                    │ submitted│
                    └────┬─────┘
                         │
                    ┌────▼─────┐
              ┌─────│ runnable │◄──────────────────────┐
              │     └────┬─────┘                        │
              │          │ claim                         │
              │     ┌────▼─────┐     resume/             │
              │     │  active  │──────────►┌──────────┐ │
              │     └──┬───┬───┘  suspend  │suspended │─┘
              │        │   │               └─────┬────┘
              │        │   │  move_to_           │ cancel/
              │        │   │  waiting_children   │ expire
              │        │   └──►(back to runnable)│
              │        │                         │
              │   ┌────▼─────┐                   │
              └──►│ terminal │◄──────────────────┘
                  └──────────┘
                  (also reachable from runnable
                   via cancel/expire/skip)
```

##### Full transition table

| From | To | Trigger | Notes |
|------|----|---------|-------|
| `submitted` | `runnable` | Engine resolves eligibility | Normal path. |
| `runnable` | `active` | Worker claims + lease acquired | Sets `ownership_state = leased`. |
| `runnable` | `terminal` | Cancel, expire, or skip | `terminal_outcome` set accordingly. |
| `active` | `terminal` | Complete, fail, cancel, expire | Clears lease. Sets `terminal_outcome`. |
| `active` | `suspended` | Worker suspends | Releases lease. Creates suspension record. See RFC-004. |
| `active` | `runnable` | Move to waiting-children, delay, or release lease | `eligibility_state` updated accordingly. |
| `suspended` | `runnable` | Resume conditions satisfied | Execution re-enters scheduling. |
| `suspended` | `terminal` | Cancel or suspension timeout | `terminal_outcome = cancelled` or `expired`. |
| `terminal` | `runnable` | Replay (explicit operator action) | Creates new attempt. Resets `terminal_outcome = none`. |

##### Forbidden transitions

| Transition | Why |
|-----------|-----|
| `terminal` → `active` | Must go through `runnable` first (replay creates new attempt). |
| `submitted` → `active` | Must resolve to `runnable` first. |
| `suspended` → `active` | Must go through `runnable` (re-enter scheduling for fair claiming). |
| Any → `submitted` | `submitted` is an entry-only phase. |
| Concurrent `active` by multiple workers | Invariant E3: single active owner. |

#### 3.2 Ownership State Transitions

| From | To | Trigger |
|------|----|---------|
| `unowned` | `leased` | Successful claim. |
| `leased` | `unowned` | Lease released (complete, fail, suspend, delay). |
| `leased` | `lease_expired_reclaimable` | Lease TTL expires without renewal. |
| `leased` | `lease_revoked` | Operator or engine revokes lease. Execution stays `active` in a non-trusted ownership state until reclaim or override. |
| `lease_expired_reclaimable` | `leased` | Another worker reclaims (new lease, new attempt, higher epoch). |
| `lease_expired_reclaimable` | `unowned` | Engine moves execution back to `runnable`. |
| `lease_revoked` | `leased` | Another worker reclaims after revocation (new lease, new attempt, higher epoch). |
| `lease_revoked` | `unowned` | Engine transitions execution out of `active` (e.g. cancel, move to runnable). |

---

### 4. Execution Operations

#### 4.1 Submission Operations (Class A — atomic)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `create_execution` | `lane_id`, `execution_kind`, `input_payload`, `policy?`, `idempotency_key?`, `tags?` | Creates execution in `submitted` phase. Engine resolves to `runnable` (possibly `delayed`). If `idempotency_key` matches existing execution within dedup window, returns existing. **Idempotency is TTL-bounded:** the dedup key (`ff:idem:<namespace>:<key>`) has `TTL = min(dedup_window, retention_window)`. After TTL expires, a retry with the same key creates a new execution. V1 default `dedup_window`: 24h. Idempotency is namespace-scoped — different tenants may safely reuse the same key. Callers requiring permanent dedup must enforce it externally before submission. |
| `create_delayed_execution` | Same + `delay_until` | Creates execution in `runnable` with `eligibility_state = not_eligible_until_time`. |
| `create_scheduled_execution` | Same + `schedule_spec` | Creates with schedule metadata. Engine handles recurring promotion. |
| `create_child_execution` | `parent_execution_id`, `flow_id`, same as above | Creates with `parent_execution_id` and `flow_id` set. **Starts with `eligibility_state = blocked_by_dependencies`**, `blocking_reason = waiting_for_children`, `blocking_detail = "waiting for dependency edges to be applied"`, `attempt_state = pending_first_attempt` — NOT added to eligible set. Added to `blocked:dependencies` set instead. This closes the race window between execution creation and `apply_dependency_to_child`. If the child has no dependencies, the caller must call `promote_blocked_to_eligible` (RFC-010 #35) after flow setup to avoid reconciler delay. Alternatively, use `create_execution` (without flow context) which starts eligible. |

#### 4.2 Claim / Ownership Operations (Class A — atomic)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `claim_execution` | `lane_id?`, `worker_id`, `capabilities`, `batch_size?` | Two-phase: (1) scheduler pre-computes routing/admission and issues a `claim_grant` key, (2) atomic Lua consumes the grant, creates lease, creates attempt, transitions to `active` + `leased` + `running_attempt`. Returns execution data + lease. See §9.3 and RFC-003. |
| `renew_lease` | `execution_id`, `lease_id`, `lease_epoch` | Extends lease TTL. Fails if stale. Does **not** create new attempt. See RFC-003. |
| `release_lease` | `execution_id`, `lease_id`, `lease_epoch` | Voluntarily releases lease. Used by complete/fail/suspend. |
| `recover_abandoned_execution` | `execution_id` | Engine-initiated. Marks previous attempt as `interrupted`. Creates new attempt with new lease. Increments `lease_epoch`. See RFC-002. |

#### 4.3 Runtime Mutation Operations (Class A — atomic)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `complete_execution` | `execution_id`, `lease_id`, `lease_epoch`, `result_payload?` | Requires valid lease. Sets `terminal_outcome = success`. Clears lease. |
| `fail_execution` | `execution_id`, `lease_id`, `lease_epoch`, `failure_reason` | Requires valid lease. If retries remain: transitions to `runnable` + `not_eligible_until_time` (backoff). If exhausted: sets `terminal_outcome = failed`. |
| `cancel_execution` | `execution_id`, `reason`, `operator?` | May bypass lease if operator-privileged. Sets `terminal_outcome = cancelled`. |
| `expire_execution` | `execution_id` | Engine-initiated on per-attempt timeout or total execution deadline. Handles three lifecycle phases: **active** (close attempt + stream, release lease), **runnable** (ZREM from current scheduling index), **suspended** (close waitpoint + suspension, terminate attempt). All paths set `terminal_outcome = expired`. See RFC-010 §6.16 for scanner details. |
| `skip_execution` | `execution_id`, `reason` | Engine-initiated when required dependency failed. Sets `terminal_outcome = skipped`. |
| `delay_execution` | `execution_id`, `lease_id`, `lease_epoch`, `delay_until` | From active: releases lease, moves to `runnable` + `not_eligible_until_time`. |
| `move_to_waiting_children` | `execution_id`, `lease_id`, `lease_epoch` | Releases lease. Sets `eligibility_state = blocked_by_dependencies`, `blocking_reason = waiting_for_children`. |
| `suspend_execution` | `execution_id`, `lease_id`, `lease_epoch`, `suspension_spec` | Releases lease. Transitions to `suspended`. Creates suspension record. See RFC-004. |
| `resume_execution` | `execution_id`, `trigger` | Engine-initiated when resume conditions met. Transitions `suspended` → `runnable`. |
| `signal_execution` | `execution_id`, `waitpoint_id?`, `signal` | Delivers signal. Engine evaluates resume conditions. See RFC-005 for signal model. |

#### 4.4 Retry / Replay Operations (Class A — atomic)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `retry_execution` | `execution_id` | Engine-initiated after `fail_execution` when retry policy allows. Creates new attempt (RFC-002). Moves to `runnable`. |
| `replay_execution` | `execution_id`, `replay_reason`, `requested_by` | Operator or system action. Execution must be `terminal`. Enforces `replay_count < max_replay_count` from execution policy (default: 10). If limit reached, returns `max_replays_exhausted`. Otherwise: sets `attempt_state = pending_replay_attempt` and stores lineage on exec core (`pending_replay_reason`, `pending_replay_requested_by`, `pending_previous_attempt_index`). Does NOT create the attempt record — `claim_execution` creates it with `attempt_type = replay`. Increments `replay_count`. **Dependency reset for skipped flow members:** If `terminal_outcome = skipped` AND `flow_id` is set (execution is a flow member that was skipped due to upstream failure), the script resets dependency edge state within `{p:N}`: for each `dep:<edge_id>` hash with `state = impossible`, set `state = unsatisfied`, SADD `edge_id` back to `deps:unresolved`, reset `deps:meta` (`impossible_required_count = 0`, recompute `unsatisfied_required_count`). Sets `eligibility_state = blocked_by_dependencies`, `blocking_reason = waiting_for_children`, `public_state = waiting_children` — NOT `eligible_now`. After the script returns, the engine dispatches cross-partition `resolve_dependency` calls for each reset edge (reads upstream execution terminal state, satisfies or re-skips the edge). The dependency resolution reconciler (§6.14) is a safety net. For non-skipped terminals (success, failed, cancelled, expired), replay sets `eligible_now` as normal — no dep reset needed. |
| `change_priority` | `execution_id`, `new_priority` | Updates priority. Re-scores in scheduling. |

#### 4.5 Output / Accounting Operations (Class B — durable append)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `update_progress` | `execution_id`, `lease_id`, `pct?`, `message?` | Updates progress snapshot. |
| `report_usage` | `execution_id`, `lease_id`, `usage_delta`, `usage_report_seq` | **Idempotent.** Caller provides a monotonically increasing `usage_report_seq` per attempt (starts at 1). Script checks `attempt_usage.last_usage_report_seq`: if `seq <= last` → `ok_already_applied` (no-op, prevents double-counting on retry). Otherwise: HINCRBY attempt + exec core counters, HSET `last_usage_report_seq`. Triggers budget check on `{b:M}` only if HINCRBY occurred. |
| `record_result` | `execution_id`, `lease_id`, `result_payload` | Sets final result (normally part of `complete_execution`). |

#### 4.6 Inspection Operations (Class C — derived/best-effort)

| Operation | Parameters | Returns |
|-----------|-----------|---------|
| `get_execution` | `execution_id`, `visibility_level?` | Full execution record at requested visibility. |
| `get_execution_state` | `execution_id` | State vector + `public_state` + `blocking_reason`. |
| `get_execution_history` | `execution_id` | Attempt summaries + key lifecycle events. |
| `get_blocking_reason` | `execution_id` | Detailed blocking explanation. |
| `get_execution_usage` | `execution_id` | Accounting summary. |
| `list_executions` | `lane_id?`, `public_state?`, `tags?`, `limit`, `cursor` | Paginated list with filters. |

---

### 5. Blocking Reason Model

Every non-progressing, non-terminal execution exposes a blocking reason. This answers the most common operator question: "why is this stuck?"

```
┌─────────────────────────────────────────────────────────────┐
│  blocking_reason             │  typical eligibility_state   │
├─────────────────────────────────────────────────────────────┤
│  waiting_for_worker          │  eligible_now                │
│  waiting_for_retry_backoff   │  not_eligible_until_time     │
│  waiting_for_resume_delay    │  not_eligible_until_time     │
│  waiting_for_delay           │  not_eligible_until_time     │
│  waiting_for_signal          │  not_applicable (suspended)  │
│  waiting_for_approval        │  not_applicable (suspended)  │
│  waiting_for_callback        │  not_applicable (suspended)  │
│  waiting_for_tool_result     │  not_applicable (suspended)  │
│  waiting_for_children        │  blocked_by_dependencies     │
│  waiting_for_budget          │  blocked_by_budget           │
│  waiting_for_quota           │  blocked_by_quota            │
│  waiting_for_capable_worker  │  blocked_by_route            │
│  waiting_for_locality_match  │  blocked_by_route            │
│  paused_by_operator          │  blocked_by_operator         │
│  paused_by_policy            │  blocked_by_lane_state       │
│  paused_by_flow_cancel       │  blocked_by_operator         │
└─────────────────────────────────────────────────────────────┘
```

When `blocking_reason` is not `none`, the engine may optionally populate an extended `blocking_detail` string with specifics (e.g. `"budget budget-abc123 exhausted: 150000/150000 tokens used"`, `"retry backoff until 2026-04-14T10:30:00Z"`).

---

### 6. Execution Result Model

#### 6.1 Final Outcome

Terminal executions carry a final outcome. The result is set atomically with the terminal transition.

| Terminal Outcome | Result Fields |
|-----------------|---------------|
| `success` | `result_payload` (opaque bytes), `result_encoding` |
| `failed` | `failure_reason` (structured string) |
| `cancelled` | `cancellation_reason` (structured string) |
| `expired` | `expiration_detail` (which deadline: TTL, suspension timeout, total deadline) |
| `skipped` | `skip_reason` (which dependency failed) |

#### 6.2 Intermediate Output

Intermediate output during execution is emitted via the attempt-scoped stream (separate primitive). The execution object itself carries only summary fields:

- `progress_pct` / `progress_message` — last snapshot
- Token/cost counters — cumulative across attempts
- Stream metadata is on the attempt record (see RFC-002)

---

### 7. Execution Lineage

#### 7.1 Stable Identity Across Retries, Reclaims, and Replays

`execution_id` never changes. All of the following create a new **attempt** on the same execution:

| Event | New Attempt? | execution_id Changes? |
|-------|-------------|----------------------|
| Initial claim | Attempt 0 | No |
| Retry after failure | Yes (attempt N+1) | No |
| Reclaim after lease expiry | Yes (attempt N+1) | No |
| Replay after terminal | Yes (attempt N+1) | No |
| Fallback progression | Yes (attempt N+1) | No |
| Lease renewal | No | No |
| Signal delivery | No | No |
| Progress update | No | No |

#### 7.2 Lineage Fields

| Field | Scope | Description |
|-------|-------|-------------|
| `execution_id` | Execution | Stable across all events. |
| `current_attempt_index` | Execution | Monotonically increasing (0-based). |
| `parent_execution_id` | Execution | Parent in flow tree. |
| `flow_id` | Execution | Flow membership. |
| `retry_count` | Execution | Total retries. |
| `reclaim_count` | Execution | Total reclaims. |
| `replay_count` | Execution | Total replays. |
| `fallback_index` | Execution | Current position in fallback chain. |

Attempt-level lineage (per-attempt worker, route, usage) is defined in RFC-002.

---

### 8. Execution Visibility Levels

FlowFabric supports three visibility levels to avoid forcing every caller to see every detail.

| Level | Audience | Includes |
|-------|----------|----------|
| `public` | External callers, client SDKs | `execution_id`, `public_state`, `blocking_reason` (category only), `progress_pct`, `result_payload`, timestamps (`created_at`, `completed_at`), `execution_kind`, `tags`. |
| `operator` | Dashboards, admin APIs | Everything in `public` plus: full state vector, `lease_epoch`, `worker_id`, attempt history summary, retry/reclaim/replay counts, usage counters, `failure_reason`, `cancellation_reason`, `last_operator_action`. |
| `internal` | Engine internals, debugging | Everything in `operator` plus: raw Valkey key references, scheduling scores, routing decision snapshots, policy snapshots, full event log pointers. |

Visibility filtering is enforced at the API layer, not in Valkey Lua scripts. All fields are stored on the execution core hash; the API server reads the full record and projects the response based on the caller's authenticated visibility level. Lua scripts operate on the full record and do not filter by visibility.

---

### 9. Valkey Data Model

#### 9.1 Partition Tag Model

All keys that must participate in one atomic Lua script share the same Valkey Cluster hash tag. FlowFabric uses a **partition tag** `{p:N}` (aligned with RFC-003) rather than per-lane or per-execution tags.

**Partition assignment:**
- `partition = hash(execution_id) % num_partitions`
- Computed once at execution creation and stored on the execution record as `partition_id`
- `num_partitions` is fixed at deployment time (e.g. 256, 1024)
- The partition tag `{p:N}` appears in all keys that must be co-located for atomic operations

This ensures execution core, lease, attempt, and partition-local indexes all hash to the same Valkey Cluster slot.

#### 9.2 Primary Execution Record

Each execution is stored as a Valkey hash:

```
Key: ff:exec:{p:N}:<execution_id>:core
```

All hash field names use their **long canonical form**. This is the authoritative field name list — all Lua scripts across all RFCs must use these exact names.

| Hash Field | Type | Notes |
|-----------|------|-------|
| `execution_id` | UUID | Stable identity |
| `partition_id` | u32 | Stored for lookups |
| `namespace` | String | |
| `lane_id` | String | |
| `execution_kind` | String | |
| `idempotency_key` | String | Optional |
| `lifecycle_phase` | String | State vector dimension A |
| `ownership_state` | String | State vector dimension B |
| `eligibility_state` | String | State vector dimension C |
| `blocking_reason` | String | State vector dimension D |
| `blocking_detail` | String | Extended human-readable explanation when blocking_reason is set. Cleared when blocking_reason=none. |
| `terminal_outcome` | String | State vector dimension E |
| `attempt_state` | String | State vector dimension F |
| `public_state` | String | Derived, stored for read efficiency |
| `priority` | i32 | |
| `current_attempt_index` | u32 | 0-based. Matches RFC-003/004 field name. |
| `total_attempt_count` | u32 | |
| `current_attempt_id` | UUID | Current attempt bound to lease |
| `current_lease_id` | UUID | Optional |
| `current_lease_epoch` | u64 | Monotonic fencing token |
| `current_worker_id` | String | Optional |
| `current_worker_instance_id` | String | Process/container identity |
| `current_lane` | String | Lane at acquisition time |
| `lease_acquired_at` | timestamp ms | |
| `lease_expires_at` | timestamp ms | Hard ownership deadline |
| `lease_expired_at` | timestamp ms | Optional, set when lease expiry detected |
| `lease_last_renewed_at` | timestamp ms | |
| `lease_renewal_deadline` | timestamp ms | |
| `lease_reclaim_count` | u32 | |
| `lease_revoked_at` | timestamp ms | Optional |
| `lease_revoke_reason` | String | Optional |
| `current_suspension_id` | UUID | Optional, set when suspended |
| `current_waitpoint_id` | UUID | Optional, set when suspended |
| `parent_execution_id` | UUID | Optional |
| `flow_id` | UUID | Optional |
| `created_at` | timestamp ms | |
| `started_at` | timestamp ms | Optional |
| `completed_at` | timestamp ms | Optional |
| `last_transition_at` | timestamp ms | |
| `last_mutation_at` | timestamp ms | |
| `delay_until` | timestamp ms | Optional |
| `total_input_tokens` | u64 | |
| `total_output_tokens` | u64 | |
| `total_thinking_tokens` | u64 | |
| `total_cost_micros` | u64 | |
| `total_latency_ms` | u64 | |
| `retry_count` | u32 | |
| `reclaim_count` | u32 | |
| `replay_count` | u32 | |
| `fallback_index` | u32 | |
| `child_count` | u32 | |
| `completed_child_count` | u32 | |
| `failed_child_count` | u32 | |
| `progress_pct` | u8 | |
| `progress_message` | String | |
| `failure_reason` | String | Optional |
| `cancellation_reason` | String | Optional |
| `creator_identity` | String | |
| `last_operator_action` | String | Optional |
| `last_operator_action_at` | timestamp ms | Optional |
| `budget_ids` | String | Comma-separated list of attached budget UUIDs. Denormalized from ExecutionPolicySnapshot for fast access during `report_usage` without JSON parsing. Empty string if no budgets attached. |
| `pending_retry_reason` | String | **Transient.** Set by `fail_execution` (retry path) with the failure reason. Consumed and cleared by `claim_execution` which copies it to the new attempt's `retry_reason` field. Empty when not pending retry. |
| `pending_previous_attempt_index` | u32 | **Transient.** Set by `fail_execution` or `replay_execution` with the prior attempt index. Consumed and cleared by `claim_execution` which copies it to the new attempt's `previous_attempt_index` or `replayed_from_attempt_index`. Empty when not pending. |
| `pending_replay_reason` | String | **Transient.** Set by `replay_execution` with the operator-supplied replay reason. Consumed and cleared by `claim_execution` which copies it to the new attempt's `replay_reason`. Empty when not pending replay. |
| `pending_replay_requested_by` | String | **Transient.** Set by `replay_execution` with the actor identity. Consumed and cleared by `claim_execution` which copies it to the new attempt's `replay_requested_by`. Empty when not pending replay. |

Payload and result are stored separately to avoid loading large blobs on every state read:

```
ff:exec:{p:N}:<execution_id>:payload    → raw bytes
ff:exec:{p:N}:<execution_id>:result     → raw bytes
ff:exec:{p:N}:<execution_id>:policy     → JSON-encoded ExecutionPolicySnapshot
ff:exec:{p:N}:<execution_id>:tags       → Valkey hash of tag key-value pairs
```

#### 9.3 Scheduling and Indexing Structures

All partition-local indexes use the same `{p:N}` hash tag, enabling atomic Lua scripts to update execution state and indexes in one call.

##### Eligible queue (priority-ordered, partition-local)

```
Key: ff:idx:{p:N}:lane:<lane_id>:eligible
Type: Sorted Set
Score: -(priority * 1_000_000_000_000) + created_at_ms
       (composite: highest priority first, FIFO within same tier)
Member: execution_id
```

Only executions with `lifecycle_phase = runnable` AND `eligibility_state = eligible_now` appear here.

##### Delayed set (time-ordered, partition-local)

```
Key: ff:idx:{p:N}:lane:<lane_id>:delayed
Type: Sorted Set
Score: delay_until timestamp (ms)
Member: execution_id
```

A periodic promotion loop (`ZRANGEBYSCORE ... -inf <now>`) moves eligible executions to the `eligible` sorted set.

##### Lease expiry set (partition-local, for reclaim scanning)

```
Key: ff:idx:{p:N}:lease_expiry
Type: Sorted Set
Score: lease_expires_at timestamp (ms)
Member: execution_id
```

A reclaim scanner uses `ZRANGEBYSCORE ... -inf <now>` to find expired leases. Shared across lanes within the partition.

##### Terminal set (completion-time-ordered, partition-local)

```
Key: ff:idx:{p:N}:lane:<lane_id>:terminal
Type: Sorted Set
Score: completed_at timestamp (ms)
Member: execution_id
```

Used for retention scanning and historical queries.

##### Blocked sets (partition-local)

```
ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies → Sorted Set (score: created_at)
ff:idx:{p:N}:lane:<lane_id>:blocked:budget       → Sorted Set
ff:idx:{p:N}:lane:<lane_id>:blocked:quota        → Sorted Set
ff:idx:{p:N}:lane:<lane_id>:blocked:route        → Sorted Set
ff:idx:{p:N}:lane:<lane_id>:blocked:operator     → Sorted Set
```

When blocking condition clears, execution moves to `eligible` or `delayed`.

##### Suspended set (partition-local)

```
Key: ff:idx:{p:N}:lane:<lane_id>:suspended
Type: Sorted Set
Score: suspension_timeout_at timestamp (ms), or MAX if no timeout
Member: execution_id
```

##### Worker-instance lease set (partition-local)

```
Key: ff:idx:{p:N}:worker:<worker_instance_id>:leases
Type: Set
Member: execution_id
```

Used for operator drain. Best-effort — authoritative ownership check is always on the execution core record.

##### Claim grant (partition-local, ephemeral)

```
Key: ff:exec:{p:N}:<execution_id>:claim_grant
Type: Hash
Fields: worker_id, worker_instance_id, lane, capability_hash, grant_expires_at
TTL: short (e.g. 5s)
```

Pre-computed by the scheduler/admission layer. Consumed atomically by the claim Lua script. See RFC-003 for grant semantics.

##### Flow-structural keys (authoritative, on `{fp:N}` partition — see RFC-007)

```
ff:flow:{fp:N}:<flow_id>:members    → Set (member: execution_id)
```

Flow membership is authoritative topology data stored on the flow partition. Maintained atomically within `{fp:N}` Lua scripts. See RFC-007 and RFC-010 §1.2 for full flow key schema.

##### Cross-partition secondary indexes (eventually consistent)

```
ff:ns:<namespace>:executions        → Sorted Set (score: created_at, member: execution_id)
ff:idem:<namespace>:<idempotency_key> → String (value: execution_id, TTL: min(dedup_window, retention_window))
ff:tag:<namespace>:<key>:<value>    → Set (member: execution_id)
```

These do not share the `{p:N}` hash tag and cannot participate in the atomic Lua scripts. They are maintained via best-effort secondary writes after the atomic transition succeeds.

#### 9.4 Atomicity Model

All state transitions use Lua scripts for atomicity. Every key in a single Lua script shares the `{p:N}` hash tag, ensuring colocation on one Valkey Cluster shard.

**KEYS/ARGV naming convention:** All Lua pseudocode in this RFC uses named references (`KEYS.exec_core`, `ARGV.lease_id`) per RFC-010 §4.8(c2). In Valkey Lua, KEYS and ARGV are positional arrays. The `-- KEYS:` comment at the top of each script defines the mapping from names to positions. Where older pseudocode uses `KEYS[N]` notation, the KEYS comment provides the authoritative name-to-position mapping. Implementers should use the named form in implementation code (via a helper that maps names to positions).

**Capability, budget, and quota validation happen in the claim-grant pre-step** (scheduler/admission layer), NOT inside the atomic Lua. The Lua script only validates and consumes the claim grant.

##### Lua script: claim_execution (new attempt)

Creates a new attempt and lease for a fresh, retried, or replayed execution. The attempt type is derived from the execution's `attempt_state` on the exec core — `fail_execution` and `replay_execution` set the state but do NOT create attempt records (preventing double-creation). Does NOT apply to resumed-after-suspension executions (see `claim_resumed_execution` below).

```lua
-- KEYS: exec_core, claim_grant, eligible_zset, lease_expiry_zset,
--       worker_leases, attempt_hash, attempt_usage, attempt_policy,
--       attempts_zset, lease_current, lease_history, active_index,
--       attempt_timeout_zset, execution_deadline_zset
-- ARGV: execution_id, worker_id, worker_instance_id, lane,
--       capability_snapshot_hash, lease_id, lease_ttl_ms,
--       renew_before_ms, attempt_id, attempt_policy_json

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)

-- 1. Validate execution state (long canonical field names)
local core = redis.call("HGETALL", KEYS[1])
if core.lifecycle_phase ~= "runnable" then return err("execution_not_leaseable") end
if core.ownership_state ~= "unowned" then return err("lease_conflict") end
if core.eligibility_state ~= "eligible_now" then return err("execution_not_leaseable") end
if core.terminal_outcome ~= "none" then return err("execution_not_leaseable") end

-- Defense-in-depth guards (Invariant A3 + dispatch correctness):
-- A3: at most one active attempt. If attempt_state=running_attempt, a bug
-- elsewhere left the state inconsistent (V9 says runnable+running=invalid).
if core.attempt_state == "running_attempt" then
  return err("active_attempt_exists")
end
-- Dispatch: resume-from-suspension must use claim_resumed_execution.
if core.attempt_state == "attempt_interrupted" then
  return err("use_claim_resumed_execution")
end

-- 2. Validate and consume claim grant
local grant = redis.call("HGETALL", KEYS[2])
if not grant or grant.worker_id ~= ARGV.worker_id then
  return err("invalid_claim_grant")
end
if tonumber(grant.grant_expires_at) < now_ms then
  return err("claim_grant_expired")
end
redis.call("DEL", KEYS[2])

-- 3. Compute lease fields
local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
local expires_at = now_ms + tonumber(ARGV.lease_ttl_ms)
local renewal_deadline = now_ms + tonumber(ARGV.renew_before_ms)
local next_att_idx = tonumber(core.total_attempt_count or "0")

-- 4. Derive attempt type from exec core attempt_state.
--    fail_execution sets pending_retry_attempt + stores lineage on exec core.
--    replay_execution sets pending_replay_attempt + stores lineage on exec core.
--    create_execution sets pending_first_attempt (initial).
local attempt_type = "initial"
local lineage_fields = {}
if core.attempt_state == "pending_retry_attempt" then
  attempt_type = "retry"
  lineage_fields = {
    "retry_reason", core.pending_retry_reason or "",
    "previous_attempt_index", core.pending_previous_attempt_index or ""
  }
elseif core.attempt_state == "pending_replay_attempt" then
  attempt_type = "replay"
  lineage_fields = {
    "replay_reason", core.pending_replay_reason or "",
    "replay_requested_by", core.pending_replay_requested_by or "",
    "replayed_from_attempt_index", core.pending_previous_attempt_index or ""
  }
end

-- 5. Create attempt record (RFC-002)
redis.call("HSET", KEYS[6],
  "attempt_id", ARGV.attempt_id,
  "execution_id", ARGV.execution_id,
  "attempt_index", next_att_idx,
  "attempt_type", attempt_type,
  "attempt_state", "started",
  "created_at", now_ms,
  "started_at", now_ms,
  "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  unpack(lineage_fields))
redis.call("ZADD", KEYS[9], now_ms, next_att_idx)
redis.call("HSET", KEYS[8], unpack_policy(ARGV.attempt_policy_json))

-- 6. Create lease record (RFC-003)
redis.call("DEL", KEYS[10])
redis.call("HSET", KEYS[10],
  "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch,
  "execution_id", ARGV.execution_id,
  "attempt_id", ARGV.attempt_id,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "acquired_at", now_ms,
  "expires_at", expires_at,
  "last_renewed_at", now_ms,
  "renewal_deadline", renewal_deadline)

-- 7. Update execution core — ALL 7 state vector dimensions
--    Clear pending lineage fields (consumed by attempt creation above).
redis.call("HSET", KEYS[1],
  "lifecycle_phase", "active",
  "ownership_state", "leased",
  "eligibility_state", "not_applicable",
  "blocking_reason", "none",
  "blocking_detail", "",
  "terminal_outcome", "none",
  "attempt_state", "running_attempt",
  "public_state", "active",
  "current_attempt_index", next_att_idx,
  "total_attempt_count", next_att_idx + 1,
  "current_attempt_id", ARGV.attempt_id,
  "current_lease_id", ARGV.lease_id,
  "current_lease_epoch", next_epoch,
  "current_worker_id", ARGV.worker_id,
  "current_worker_instance_id", ARGV.worker_instance_id,
  "current_lane", ARGV.lane,
  "lease_acquired_at", now_ms,
  "lease_expires_at", expires_at,
  "lease_last_renewed_at", now_ms,
  "lease_renewal_deadline", renewal_deadline,
  "started_at", core.started_at or now_ms,
  "pending_retry_reason", "",
  "pending_replay_reason", "",
  "pending_replay_requested_by", "",
  "pending_previous_attempt_index", "",
  "last_transition_at", now_ms, "last_mutation_at", now_ms)

-- 8. Update indexes
redis.call("ZREM", KEYS[3], ARGV.execution_id)       -- remove from eligible
redis.call("ZADD", KEYS[4], expires_at, ARGV.execution_id) -- add to lease_expiry
redis.call("SADD", KEYS[5], ARGV.execution_id)       -- add to worker leases
redis.call("ZADD", KEYS[12], expires_at, ARGV.execution_id) -- add to per-lane active

-- 8a. Timeout indexes (§6.16)
if ARGV.attempt_timeout_ms ~= "" and ARGV.attempt_timeout_ms ~= "0" then
  redis.call("ZADD", KEYS[13], now_ms + tonumber(ARGV.attempt_timeout_ms), ARGV.execution_id)
end
if ARGV.execution_deadline_at ~= "" and ARGV.execution_deadline_at ~= "0" then
  redis.call("ZADD", KEYS[14], tonumber(ARGV.execution_deadline_at), ARGV.execution_id)
end

-- 9. Lease history event (includes attempt_index)
redis.call("XADD", KEYS[11], "*",
  "event", "acquired", "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch, "attempt_id", ARGV.attempt_id,
  "attempt_index", next_att_idx,
  "worker_id", ARGV.worker_id, "ts", now_ms)

return ok(ARGV.lease_id, next_epoch, expires_at, ARGV.attempt_id, next_att_idx)
```

##### Lua script: claim_resumed_execution (same attempt continues)

Used after a suspended execution is resumed (signal satisfied the waitpoint, execution returned to `runnable`). Does NOT create a new attempt — continues the existing one. Issues a new lease with incremented `lease_epoch`, binding it to the existing `attempt_index`.

```lua
-- KEYS: exec_core, claim_grant, eligible_zset, lease_expiry_zset,
--       worker_leases, existing_attempt_hash, lease_current, lease_history,
--       active_index, attempt_timeout_zset, execution_deadline_zset
-- ARGV: execution_id, worker_id, worker_instance_id, lane,
--       capability_snapshot_hash, lease_id, lease_ttl_ms,
--       renew_before_ms

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)

-- 1. Validate execution state
local core = redis.call("HGETALL", KEYS[1])
if core.lifecycle_phase ~= "runnable" then return err("execution_not_leaseable") end
if core.ownership_state ~= "unowned" then return err("lease_conflict") end
if core.eligibility_state ~= "eligible_now" then return err("execution_not_leaseable") end
if core.terminal_outcome ~= "none" then return err("execution_not_leaseable") end

-- 2. Validate this is a resumed-after-suspension execution
if core.attempt_state ~= "attempt_interrupted" then
  return err("not_a_resumed_execution")
end

-- 3. Validate and consume claim grant
local grant = redis.call("HGETALL", KEYS[2])
if not grant or grant.worker_id ~= ARGV.worker_id then
  return err("invalid_claim_grant")
end
if tonumber(grant.grant_expires_at) < now_ms then
  return err("claim_grant_expired")
end
redis.call("DEL", KEYS[2])

-- 4. Compute lease fields — new lease, same attempt
local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
local expires_at = now_ms + tonumber(ARGV.lease_ttl_ms)
local renewal_deadline = now_ms + tonumber(ARGV.renew_before_ms)
local existing_att_idx = tonumber(core.current_attempt_index)
local existing_attempt_id = core.current_attempt_id

-- 5. Transition existing attempt back to started (was interrupted)
-- Preserve original started_at — the attempt was started before suspension.
-- Record resumed_at for the resume event timestamp.
redis.call("HSET", KEYS[6],
  "attempt_state", "started",
  "resumed_at", now_ms,
  "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id)

-- 6. Create lease record (RFC-003)
redis.call("DEL", KEYS[7])
redis.call("HSET", KEYS[7],
  "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch,
  "execution_id", ARGV.execution_id,
  "attempt_id", existing_attempt_id,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "acquired_at", now_ms,
  "expires_at", expires_at,
  "last_renewed_at", now_ms,
  "renewal_deadline", renewal_deadline)

-- 7. Update execution core — ALL 7 state vector dimensions
redis.call("HSET", KEYS[1],
  "lifecycle_phase", "active",
  "ownership_state", "leased",
  "eligibility_state", "not_applicable",
  "blocking_reason", "none",
  "blocking_detail", "",
  "terminal_outcome", "none",
  "attempt_state", "running_attempt",
  "public_state", "active",
  "current_lease_id", ARGV.lease_id,
  "current_lease_epoch", next_epoch,
  "current_worker_id", ARGV.worker_id,
  "current_worker_instance_id", ARGV.worker_instance_id,
  "current_lane", ARGV.lane,
  "lease_acquired_at", now_ms,
  "lease_expires_at", expires_at,
  "lease_last_renewed_at", now_ms,
  "lease_renewal_deadline", renewal_deadline,
  "last_transition_at", now_ms, "last_mutation_at", now_ms)

-- 8. Update indexes
redis.call("ZREM", KEYS[3], ARGV.execution_id)       -- remove from eligible
redis.call("ZADD", KEYS[4], expires_at, ARGV.execution_id) -- add to lease_expiry
redis.call("SADD", KEYS[5], ARGV.execution_id)       -- add to worker leases
redis.call("ZADD", KEYS[9], expires_at, ARGV.execution_id) -- add to per-lane active

-- 8a. Timeout indexes (§6.16) — use remaining timeout from pre-suspension
if ARGV.attempt_timeout_at ~= "" and ARGV.attempt_timeout_at ~= "0" then
  redis.call("ZADD", KEYS[10], tonumber(ARGV.attempt_timeout_at), ARGV.execution_id)
end
if ARGV.execution_deadline_at ~= "" and ARGV.execution_deadline_at ~= "0" then
  redis.call("ZADD", KEYS[11], tonumber(ARGV.execution_deadline_at), ARGV.execution_id)
end

-- 9. Lease history event (includes attempt_index)
redis.call("XADD", KEYS[8], "*",
  "event", "acquired", "lease_id", ARGV.lease_id,
  "lease_epoch", next_epoch, "attempt_id", existing_attempt_id,
  "attempt_index", existing_att_idx,
  "worker_id", ARGV.worker_id, "reason", "resume_after_suspension",
  "ts", now_ms)

return ok(ARGV.lease_id, next_epoch, expires_at, existing_attempt_id, existing_att_idx)
```

##### Lua script: complete_execution

```lua
-- KEYS: exec_core, attempt_hash, lease_expiry_zset, worker_leases,
--       terminal_zset, lease_current, lease_history, active_index,
--       stream_meta, result_key, attempt_timeout_zset, execution_deadline_zset
-- ARGV: execution_id, lease_id, lease_epoch, attempt_id, result_payload

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.exec_core)

-- Validate lease (long canonical field names)
-- Enriched error per §4.9: return terminal_outcome + lease_epoch so client
-- can distinguish "my completion already won" from "someone else terminated"
if core.lifecycle_phase ~= "active" then
  return err("execution_not_active",
    core.terminal_outcome or "", core.current_lease_epoch or "")
end
if core.ownership_state == "lease_revoked" then return err("lease_revoked") end
if tonumber(core.lease_expires_at) <= now_ms then return err("lease_expired") end
if core.current_lease_id ~= ARGV.lease_id then return err("stale_lease") end
if core.current_lease_epoch ~= ARGV.lease_epoch then return err("stale_lease") end
if core.current_attempt_id ~= ARGV.attempt_id then return err("stale_lease") end

-- Update attempt to terminal
redis.call("HSET", KEYS.attempt_hash,
  "attempt_state", "ended_success", "ended_at", now_ms)

-- Close attempt stream if exists (RFC-006)
if redis.call("EXISTS", KEYS.stream_meta) == 1 then
  redis.call("HSET", KEYS.stream_meta, "closed_at", now_ms, "closed_reason", "attempt_success")
end

-- Store result atomically with state transition (not a separate call)
if ARGV.result_payload ~= "" then
  redis.call("SET", KEYS.result_key, ARGV.result_payload)
end

-- Update execution core — ALL 7 state vector dimensions
redis.call("HSET", KEYS.exec_core,
  "lifecycle_phase", "terminal",
  "ownership_state", "unowned",
  "eligibility_state", "not_applicable",
  "blocking_reason", "none",
  "blocking_detail", "",
  "terminal_outcome", "success",
  "attempt_state", "attempt_terminal",
  "public_state", "completed",
  "completed_at", now_ms,
  "last_transition_at", now_ms, "last_mutation_at", now_ms,
  "current_lease_id", "", "current_lease_epoch", core.current_lease_epoch,
  "current_worker_id", "", "current_worker_instance_id", "",
  "lease_expires_at", "", "lease_last_renewed_at", "",
  "lease_renewal_deadline", "", "lease_revoked_at", "", "lease_revoke_reason", "")

-- Update indexes
redis.call("ZREM", KEYS.lease_expiry_zset, ARGV.execution_id)
redis.call("SREM", KEYS.worker_leases, ARGV.execution_id)
redis.call("ZADD", KEYS.terminal_zset, now_ms, ARGV.execution_id)
redis.call("ZREM", KEYS.active_index, ARGV.execution_id)
redis.call("ZREM", KEYS.attempt_timeout_zset, ARGV.execution_id)
redis.call("ZREM", KEYS.execution_deadline_zset, ARGV.execution_id)

-- Clean up lease
redis.call("DEL", KEYS.lease_current)
redis.call("XADD", KEYS.lease_history, "*",
  "event", "released", "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch, "attempt_index", core.current_attempt_index,
  "reason", "completed", "ts", now_ms)

return ok()
```

##### Lua script: fail_execution (with retry decision)

```lua
-- KEYS: exec_core, attempt_hash, lease_expiry_zset, worker_leases,
--       terminal_zset, delayed_zset, lease_current, lease_history,
--       active_index, stream_meta, attempt_timeout_zset, execution_deadline_zset
-- ARGV: execution_id, lease_id, lease_epoch, attempt_id, failure_reason,
--       failure_category, max_retries, backoff_ms

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS[1])

-- Validate lease (long canonical field names)
-- Enriched error per §4.9: see complete_execution for rationale
if core.lifecycle_phase ~= "active" then
  return err("execution_not_active",
    core.terminal_outcome or "", core.current_lease_epoch or "")
end
if core.ownership_state == "lease_revoked" then return err("lease_revoked") end
if tonumber(core.lease_expires_at) <= now_ms then return err("lease_expired") end
if core.current_lease_id ~= ARGV.lease_id then return err("stale_lease") end
if core.current_lease_epoch ~= ARGV.lease_epoch then return err("stale_lease") end
if core.current_attempt_id ~= ARGV.attempt_id then return err("stale_lease") end

-- End current attempt — stays ended_failure (no superseded overwrite)
redis.call("HSET", KEYS[2],
  "attempt_state", "ended_failure",
  "ended_at", now_ms,
  "failure_reason", ARGV.failure_reason,
  "failure_category", ARGV.failure_category)

-- Close attempt stream if exists (RFC-006)
if redis.call("EXISTS", KEYS[10]) == 1 then
  redis.call("HSET", KEYS[10], "closed_at", now_ms, "closed_reason", "attempt_failure")
end

local retry_ct = tonumber(core.retry_count or "0")
local att_ct = tonumber(core.total_attempt_count or "0")
local can_retry = retry_ct < tonumber(ARGV.max_retries)

if can_retry then
  -- DO NOT create attempt record here. claim_execution creates the attempt
  -- with the correct type (retry) when a worker claims the execution.
  -- This prevents double attempt creation (fail creates one, claim creates another).

  -- Transition execution to delayed (backoff)
  -- Store retry lineage on exec core so claim_execution can copy to the attempt.
  local delay_until = now_ms + tonumber(ARGV.backoff_ms)
  -- ALL 7 state vector dimensions
  redis.call("HSET", KEYS[1],
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "not_eligible_until_time",
    "blocking_reason", "waiting_for_retry_backoff",
    "blocking_detail", "retry backoff until " .. delay_until .. " (attempt " .. (retry_ct + 1) .. " of " .. ARGV.max_retries .. ")",
    "terminal_outcome", "none",
    "attempt_state", "pending_retry_attempt",
    "public_state", "delayed",
    "current_attempt_index", tonumber(core.current_attempt_index),
    "total_attempt_count", att_ct,
    "retry_count", retry_ct + 1,
    "delay_until", delay_until,
    "pending_retry_reason", ARGV.failure_reason,
    "pending_previous_attempt_index", tonumber(core.current_attempt_index),
    "current_attempt_id", "",
    "current_lease_id", "", "current_lease_epoch", core.current_lease_epoch,
    "current_worker_id", "", "current_worker_instance_id", "",
    "lease_expires_at", "", "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "last_transition_at", now_ms, "last_mutation_at", now_ms)

  redis.call("ZREM", KEYS[3], ARGV.execution_id)     -- remove from lease_expiry
  redis.call("SREM", KEYS[4], ARGV.execution_id)     -- remove from worker leases
  redis.call("ZADD", KEYS[6], delay_until, ARGV.execution_id) -- add to delayed
  redis.call("ZREM", KEYS[9], ARGV.execution_id)    -- remove from per-lane active
  redis.call("ZREM", KEYS[11], ARGV.execution_id)   -- remove old attempt_timeout (§6.16)
  -- Note: new attempt timeout ZADD happens on next claim, not here

  redis.call("DEL", KEYS[7])
  redis.call("XADD", KEYS[8], "*",
    "event", "released", "lease_id", ARGV.lease_id,
    "attempt_index", core.current_attempt_index,
    "reason", "failed_retry_scheduled", "ts", now_ms)

  return ok("retry_scheduled", delay_until)
else
  -- Terminal failure — no more retries
  -- ALL 7 state vector dimensions
  redis.call("HSET", KEYS[1],
    "lifecycle_phase", "terminal",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "failed",
    "attempt_state", "attempt_terminal",
    "public_state", "failed",
    "failure_reason", ARGV.failure_reason,
    "completed_at", now_ms,
    "current_lease_id", "", "current_lease_epoch", core.current_lease_epoch,
    "current_worker_id", "", "current_worker_instance_id", "",
    "lease_expires_at", "", "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "last_transition_at", now_ms, "last_mutation_at", now_ms)

  redis.call("ZREM", KEYS[3], ARGV.execution_id)
  redis.call("SREM", KEYS[4], ARGV.execution_id)
  redis.call("ZADD", KEYS[5], now_ms, ARGV.execution_id)
  redis.call("ZREM", KEYS[9], ARGV.execution_id)    -- remove from per-lane active
  redis.call("ZREM", KEYS[11], ARGV.execution_id)   -- remove from attempt_timeout (§6.16)
  redis.call("ZREM", KEYS[12], ARGV.execution_id)   -- remove from execution_deadline (§6.16)

  redis.call("DEL", KEYS[7])
  redis.call("XADD", KEYS[8], "*",
    "event", "released", "lease_id", ARGV.lease_id,
    "attempt_index", core.current_attempt_index,
    "reason", "failed_terminal", "ts", now_ms)

  return ok("terminal_failed")
end
```

##### Lua script: move_to_waiting_children

Worker blocks its active execution on child dependencies. Releases the lease and transitions execution to `runnable` + `blocked_by_dependencies`. The same attempt continues — it is not ended, only paused (like suspension). When all child dependencies resolve, the execution becomes eligible and is re-claimed via `claim_resumed_execution` (same attempt continues).

```lua
-- KEYS: exec_core, attempt_hash, lease_current, lease_history,
--       lease_expiry_zset, worker_leases, active_index,
--       blocked_deps_zset, attempt_timeout_zset
-- ARGV: execution_id, lease_id, lease_epoch, attempt_id

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS[1])

-- Validate lease (long canonical field names)
if core.lifecycle_phase ~= "active" then
  return err("execution_not_active",
    core.terminal_outcome or "", core.current_lease_epoch or "")
end
if core.ownership_state == "lease_revoked" then return err("lease_revoked") end
if tonumber(core.lease_expires_at) <= now_ms then return err("lease_expired") end
if core.current_lease_id ~= ARGV.lease_id then return err("stale_lease") end
if core.current_lease_epoch ~= ARGV.lease_epoch then return err("stale_lease") end
if core.current_attempt_id ~= ARGV.attempt_id then return err("stale_lease") end

-- Pause the attempt: started → suspended (paused for children, not ended)
redis.call("HSET", KEYS[2],
  "attempt_state", "suspended",
  "suspended_at", now_ms,
  "suspension_id", "waiting_children")

-- Release lease
redis.call("DEL", KEYS[3])
redis.call("ZREM", KEYS[5], ARGV.execution_id)       -- remove from lease_expiry
redis.call("SREM", KEYS[6], ARGV.execution_id)       -- remove from worker leases
redis.call("ZREM", KEYS[7], ARGV.execution_id)       -- remove from per-lane active
redis.call("ZREM", KEYS[9], ARGV.execution_id)       -- remove from attempt_timeout (§6.16)

-- Lease history event
redis.call("XADD", KEYS[4], "*",
  "event", "released", "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch,
  "attempt_index", core.current_attempt_index,
  "attempt_id", ARGV.attempt_id,
  "reason", "waiting_children", "ts", now_ms)

-- Update execution core — ALL 7 state vector dimensions
redis.call("HSET", KEYS[1],
  "lifecycle_phase", "runnable",
  "ownership_state", "unowned",
  "eligibility_state", "blocked_by_dependencies",
  "blocking_reason", "waiting_for_children",
  "blocking_detail", "waiting for child executions to complete",
  "terminal_outcome", "none",
  "attempt_state", "attempt_interrupted",
  "public_state", "waiting_children",
  "current_lease_id", "", "current_lease_epoch", core.current_lease_epoch,
  "current_worker_id", "", "current_worker_instance_id", "",
  "lease_expires_at", "", "lease_last_renewed_at", "",
  "lease_renewal_deadline", "",
  "last_transition_at", now_ms, "last_mutation_at", now_ms)

-- Add to blocked:dependencies set
redis.call("ZADD", KEYS[8], tonumber(core.created_at or "0"), ARGV.execution_id)

return ok()
```

##### Lua script: delay_execution

Worker explicitly delays its own active execution. Releases the lease and transitions to `runnable` + `not_eligible_until_time`. The same attempt continues (paused, not ended) — when the delay expires, the promoter moves to eligible and the execution is re-claimed via `claim_resumed_execution`.

```lua
-- KEYS: exec_core, attempt_hash, lease_current, lease_history,
--       lease_expiry_zset, worker_leases, active_index,
--       delayed_zset, attempt_timeout_zset
-- ARGV: execution_id, lease_id, lease_epoch, attempt_id, delay_until

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS[1])

-- Validate lease (full template)
if core.lifecycle_phase ~= "active" then
  return err("execution_not_active",
    core.terminal_outcome or "", core.current_lease_epoch or "")
end
if core.ownership_state == "lease_revoked" then return err("lease_revoked") end
if tonumber(core.lease_expires_at) <= now_ms then return err("lease_expired") end
if core.current_lease_id ~= ARGV.lease_id then return err("stale_lease") end
if core.current_lease_epoch ~= ARGV.lease_epoch then return err("stale_lease") end
if core.current_attempt_id ~= ARGV.attempt_id then return err("stale_lease") end

-- OOM-SAFE WRITE ORDERING: exec_core FIRST (point of no return)
-- ALL 7 state vector dimensions
redis.call("HSET", KEYS[1],
  "lifecycle_phase", "runnable",
  "ownership_state", "unowned",
  "eligibility_state", "not_eligible_until_time",
  "blocking_reason", "waiting_for_delay",
  "blocking_detail", "delayed until " .. ARGV.delay_until,
  "terminal_outcome", "none",
  "attempt_state", "attempt_interrupted",
  "public_state", "delayed",
  "delay_until", ARGV.delay_until,
  "current_lease_id", "", "current_lease_epoch", core.current_lease_epoch,
  "current_worker_id", "", "current_worker_instance_id", "",
  "lease_expires_at", "", "lease_last_renewed_at", "",
  "lease_renewal_deadline", "",
  "last_transition_at", now_ms, "last_mutation_at", now_ms)

-- Pause the attempt: started → suspended (paused for delay, not ended)
redis.call("HSET", KEYS[2],
  "attempt_state", "suspended",
  "suspended_at", now_ms,
  "suspension_id", "worker_delay")

-- Release lease + update indexes
redis.call("DEL", KEYS[3])
redis.call("ZREM", KEYS[5], ARGV.execution_id)       -- remove from lease_expiry
redis.call("SREM", KEYS[6], ARGV.execution_id)       -- remove from worker leases
redis.call("ZREM", KEYS[7], ARGV.execution_id)       -- remove from per-lane active
redis.call("ZREM", KEYS[9], ARGV.execution_id)       -- remove from attempt_timeout

-- Add to delayed set
redis.call("ZADD", KEYS[8], tonumber(ARGV.delay_until), ARGV.execution_id)

-- Lease history event
redis.call("XADD", KEYS[4], "*",
  "event", "released", "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch,
  "attempt_index", core.current_attempt_index,
  "attempt_id", ARGV.attempt_id,
  "reason", "worker_delay", "ts", now_ms)

return ok()
```

##### Lua script: replay_execution

Operator-initiated replay of a terminal execution. Validates terminal state, enforces replay limits, sets pending replay lineage on exec core (does NOT create the attempt — `claim_execution` creates it with `attempt_type = replay`). For skipped flow members, resets dependency edge state so the dependency graph is re-evaluated.

```lua
-- KEYS: exec_core, terminal_zset, eligible_zset, blocked_deps_zset,
--       lease_history, deps_meta, deps_unresolved
-- ARGV: execution_id, replay_reason, requested_by, max_replay_count

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS[1])

-- Validate terminal
if core.lifecycle_phase ~= "terminal" then
  return err("execution_not_terminal")
end

-- Check replay limit
local replay_ct = tonumber(core.replay_count or "0")
if replay_ct >= tonumber(ARGV.max_replay_count) then
  return err("max_replays_exhausted")
end

-- Determine target state based on flow membership + terminal outcome
local target_eligible = true  -- default: add to eligible set
local is_skipped_flow_member = (core.terminal_outcome == "skipped"
  and core.flow_id ~= nil and core.flow_id ~= "")

if is_skipped_flow_member then
  -- Reset dependency edges: impossible → unsatisfied
  -- Scan dep:<edge_id> hashes within {p:N} for this execution
  local deps_meta = redis.call("HGETALL", KEYS[6])
  if deps_meta and deps_meta.flow_id then
    -- Reset impossible edges back to unsatisfied
    local impossible_ct = tonumber(deps_meta.impossible_required_count or "0")
    if impossible_ct > 0 then
      -- Re-read unresolved set to find edges to reset
      -- (impossible edges were already SREMd from unresolved by resolve_dependency)
      -- We need to scan dep:<edge_id> hashes — use KEYS passed by caller
      -- The caller discovers edge_ids via SCAN ff:exec:{p:N}:<eid>:dep:*
      -- and passes them as additional KEYS. For pseudocode, assume ARGV
      -- contains edge_ids to reset.
      for _, edge_id in ipairs(ARGV.edge_ids_to_reset or {}) do
        local dep_key = "ff:exec:{p:" .. core.partition_id .. "}:" ..
          ARGV.execution_id .. ":dep:" .. edge_id
        local dep = redis.call("HGETALL", dep_key)
        if dep and dep.state == "impossible" then
          redis.call("HSET", dep_key, "state", "unsatisfied", "last_resolved_at", "")
          redis.call("SADD", KEYS[7], edge_id)  -- re-add to deps:unresolved
        end
      end
      -- Recompute deps:meta counts
      local unresolved_count = redis.call("SCARD", KEYS[7])
      redis.call("HSET", KEYS[6],
        "impossible_required_count", 0,
        "unsatisfied_required_count", unresolved_count,
        "last_dependency_update_at", now_ms)
    end
  end
  target_eligible = false  -- goes to blocked:deps, not eligible
end

-- HSET exec_core — ALL 7 state vector dimensions
if target_eligible then
  redis.call("HSET", KEYS[1],
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "eligible_now",
    "blocking_reason", "waiting_for_worker",
    "blocking_detail", "",
    "terminal_outcome", "none",
    "attempt_state", "pending_replay_attempt",
    "public_state", "waiting",
    "replay_count", replay_ct + 1,
    "completed_at", "",
    "failure_reason", "",
    "cancellation_reason", "",
    "pending_replay_reason", ARGV.replay_reason,
    "pending_replay_requested_by", ARGV.requested_by,
    "pending_previous_attempt_index", core.current_attempt_index or "",
    "last_transition_at", now_ms, "last_mutation_at", now_ms)
else
  redis.call("HSET", KEYS[1],
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "blocked_by_dependencies",
    "blocking_reason", "waiting_for_children",
    "blocking_detail", "replay of skipped execution — re-evaluating dependencies",
    "terminal_outcome", "none",
    "attempt_state", "pending_replay_attempt",
    "public_state", "waiting_children",
    "replay_count", replay_ct + 1,
    "completed_at", "",
    "failure_reason", "",
    "cancellation_reason", "",
    "pending_replay_reason", ARGV.replay_reason,
    "pending_replay_requested_by", ARGV.requested_by,
    "pending_previous_attempt_index", core.current_attempt_index or "",
    "last_transition_at", now_ms, "last_mutation_at", now_ms)
end

-- Update indexes
redis.call("ZREM", KEYS[2], ARGV.execution_id)  -- remove from terminal
if target_eligible then
  local priority = tonumber(core.priority or "0")
  local created_at = tonumber(core.created_at or "0")
  redis.call("ZADD", KEYS[3],
    0 - (priority * 1000000000000) + created_at, ARGV.execution_id)
else
  redis.call("ZADD", KEYS[4],
    tonumber(core.created_at or "0"), ARGV.execution_id)
end

-- Lease history event
redis.call("XADD", KEYS[5], "*",
  "event", "replayed",
  "replay_reason", ARGV.replay_reason,
  "requested_by", ARGV.requested_by,
  "from_attempt_index", core.current_attempt_index or "",
  "ts", now_ms)

return ok(replay_ct + 1)
```

#### 9.5 Key Expiry and Retention

Terminal execution records are retained according to lane retention policy. The retention scanner periodically scans each partition's `ff:idx:{p:N}:lane:<lane_id>:terminal` and removes records older than the retention window. Payload, result, tags, policy sub-keys, dependency keys, and attempt records all share the execution's lifecycle and are purged together.

---

### 10. Error Model

| Error | When |
|-------|------|
| `execution_not_found` | GET/mutation on nonexistent execution_id. |
| `duplicate_submission` | Idempotency key collision (returns existing execution). |
| `invalid_state_transition` | Operation not valid for current lifecycle_phase. |
| `stale_lease` | Lease ID or epoch does not match current. Worker lost ownership. |
| `lease_expired` | Lease TTL passed. |
| `lease_not_held` | Operation requires lease but execution is unowned. |
| `execution_not_active` | Operation requires `active` but execution is in another phase. Returns enriched context: `{0, "execution_not_active", terminal_outcome, lease_epoch}`. Client compares its lease_epoch with returned epoch — match + `terminal_outcome=success` means "my completion already won"; mismatch or different outcome means "someone else terminated it." |
| `execution_not_terminal` | Replay requires `terminal` but execution is not. |
| `execution_already_terminal` | Complete/fail on already-terminal execution. |
| `budget_exceeded` | Budget check failed during claim or usage report. |
| `quota_exceeded` | Quota/rate-limit check failed during claim. |
| `no_eligible_execution` | Claim found nothing matching worker capabilities. |
| `capability_mismatch` | Worker lacks required capabilities for this execution. |
| `execution_not_leaseable` | Claim attempted but execution is not runnable, not eligible, not unowned, or not non-terminal. |
| `lease_conflict` | Claim attempted but a valid lease already exists. |
| `invalid_claim_grant` | Claim-grant key missing, expired, or worker identity mismatch. |
| `claim_grant_expired` | Claim-grant TTL elapsed before `acquire_lease` consumed it. |
| `not_a_resumed_execution` | `claim_resumed_execution` called but `attempt_state != attempt_interrupted`. |
| `lease_revoked` | Active-state mutation attempted but the lease was revoked by operator or engine. |
| `policy_violation` | Operation violates execution or lane policy. |
| `invalid_execution_spec` | Malformed creation request. |

---

## Interactions with Other Primitives

| Primitive | RFC | Interaction with Execution |
|-----------|-----|---------------------------|
| **Attempt** | RFC-002 | Each execution has one or more attempts. `attempt_index` and `attempt_state` live on the execution state vector. Full attempt records (timing, route, usage attribution) are defined in RFC-002. |
| **Lease** | RFC-003 | `ownership_state`, `lease_id`, `lease_epoch`, `lease_expires_at` on the execution record reference the lease primitive. Lease acquisition/renewal/revocation/reclaim semantics are defined in RFC-003. |
| **Suspension / Waitpoint** | RFC-004 | `suspend_execution` creates a suspension record. Resume transitions `suspended` → `runnable`. Waitpoint model and pending waitpoints defined in RFC-004. |
| **Signal** | RFC-005 | `signal_execution` delivers to a waitpoint. Resume condition evaluation and `suspended` → `runnable` transition in signal delivery Lua script. |
| **Stream** | RFC-006 | Attempt-scoped output streams. Execution exposes merged stream view. Stream close is atomic with attempt termination. |
| **Flow** | RFC-007 | `flow_id`, `parent_execution_id` link execution to flow coordination. Dependency satisfaction makes executions eligible. `skip_execution` triggered by dependency failure propagation. |
| **Budget / Quota** | RFC-008 | `budget_ids` on policy. `eligibility_state = blocked_by_budget` / `blocked_by_quota` when exhausted. Usage reporting increments budget counters. |
| **Scheduling / Lane** | RFC-009 | `lane_id` is the submission surface. Lane state affects `eligibility_state = blocked_by_lane_state`. Claim-grant model, priority ordering, fairness, capability matching. |

---

## V1 Scope

### In V1

- Full execution object with all fields defined in §1.2
- 6-dimension orthogonal state vector
- Deterministic `public_state` derivation
- All 11 public states including `skipped`
- Validity constraint enforcement
- All submission, claim, runtime mutation, retry/replay, and inspection operations
- Valkey hash-based execution storage
- Sorted-set-based scheduling (eligible, delayed, active, terminal, blocked, suspended)
- Lua-script atomic state transitions
- Idempotency via `SET NX` with TTL
- Three visibility levels
- Usage/accounting fields (token, cost, latency)
- Full error model

### Designed-for but deferred

- Recurring scheduled execution (cron semantics) — interface exists, scheduler loop deferred
- Bulk submission API — individual `create_execution` sufficient for v1
- Tag-based search indexes — optional, may start without
- Advanced retention policies — v1 uses simple TTL
- Cross-lane fairness scheduler — v1 uses per-lane priority ordering
- Soft-limit budgets with escalation workflows

---

## Open Questions

1. **Reclaim grace period:** Should there be a configurable grace period between lease expiry and reclaim eligibility, to handle clock skew and slow heartbeats?

**Closed questions:**
- ~~Promotion loop frequency~~ — Resolved in RFC-010 §6.10: 500ms–1s per partition (latency-sensitive).
- ~~Partition count~~ — Resolved in RFC-010 §2.3: 256 execution, 64 flow, 32 budget, 32 quota. Fixed at deployment.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 1, Sections 2.0–2.9, Finalization §1–§13
- RFC-002: Attempt Model (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
- RFC-004: Suspension and Signal Semantics (later batch)
