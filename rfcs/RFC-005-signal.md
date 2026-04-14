# RFC-005: Signal Model and Control Boundary

**Status:** Draft
**Author:** Worker-2 (Client & Public API Specialist)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 4, Finalization §7 (waitpoint model), §8 (signal vs control boundary)

---

## Summary

This RFC defines the **Signal** primitive — the durable external input mechanism used to wake, satisfy, or annotate a waiting/suspended execution context. It establishes the **signal vs control boundary** (a locked design decision): signals carry resume/data semantics (approvals, callbacks, tool results); explicit control operations carry active mutation semantics (cancel, revoke, reroute, pause). Signals are scoped to executions and waitpoints, not used as a generic mutation channel. This RFC defines the signal object, delivery model, effect model, consumption semantics, matching rules, and the Valkey data model with atomic Lua scripts for signal delivery and resume condition evaluation.

## Motivation

FlowFabric executions suspend intentionally — waiting for human approval, external callbacks, tool completions, or multi-condition coordination. The engine needs a durable, auditable, targeted input mechanism to satisfy those wait conditions and resume work. Without it:

- Callbacks arrive but are lost because no durable record exists
- Approvals race with suspension commits and produce undefined behavior
- Operators cannot inspect what signals arrived and what effect they had
- Duplicate webhook retries create duplicate resume events
- There is no clear boundary between "data that wakes a waitpoint" and "commands that mutate active execution"

**Driving use cases:** UC-19 (suspend awaiting signal), UC-20 (resume from external callback), UC-21 (human approval gate), UC-22 (wait for multiple conditions), UC-23 (step continuation), UC-25 (external tool wait), UC-42 (tool-calling agent step), UC-47 (human-in-the-loop generation).

---

## Detailed Design

### 1. Signal vs Control Boundary

**Locked decision.** For maintainability, FlowFabric draws a hard line between resume/data signals and explicit control operations.

#### Signals are for resume/data delivery

| Use | Example |
|-----|---------|
| Approval result | Human clicks "approve" or "reject" |
| Callback payload | Webhook from external system delivers result |
| Tool result | External tool completes and returns output |
| Continue/resume input | Step continuation data tied to a waitpoint |
| Custom domain event | Application-specific resume trigger |

Signals target **suspended executions** or **specific waitpoints**. They carry data. They do not mutate active execution state directly.

#### Explicit control operations are for active mutation

| Operation | Purpose |
|-----------|---------|
| `cancel_execution` | Terminate execution intentionally |
| `revoke_lease` | Invalidate active ownership |
| `reroute_execution` | Change routing target |
| `pause_lane` | Freeze intake on a lane |
| `operator_override` | Force state change with audit |

Control operations may bypass lease requirements when operator-privileged. They are defined in RFC-001 and RFC-003.

#### Why this boundary matters

- **Easier invariants:** Signal delivery cannot accidentally cancel or complete an execution.
- **Clearer audit:** "Signal arrived" and "operator cancelled" are distinct event categories.
- **Less ambiguity:** Workers and clients know that signals are safe resume-data inputs, not commands.
- **Simpler client expectations:** Sending a signal cannot produce arbitrary side effects beyond resume evaluation.

Attempts to use the signal API for active control (e.g. sending a "cancel" signal to an active execution) must be rejected with `target_not_signalable`.

---

### 2. Signal Object Definition

A signal is a **durable, addressable input event** delivered to a target execution or waitpoint. The engine records the signal, applies delivery rules, and evaluates whether it changes execution eligibility or state.

#### 2.1 Signal Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `signal_id` | `UUID` | yes | Globally unique identifier for this signal record. |
| `target_execution_id` | `UUID` | yes | The execution this signal targets. |
| `target_waitpoint_id` | `UUID` | cond. | The specific waitpoint within the execution. Required for resume/data signals when the execution has an active or pending waitpoint. |
| `target_waitpoint_key` | `String` | no | External opaque handle for the waitpoint. Self-routing: encodes partition, execution_id, waitpoint_id, and MAC (see RFC-004 §waitpoint key routing). Signal ingress decodes the key to extract partition and waitpoint_id — no lookup required. |
| `target_scope` | `TargetScope` | yes | Delivery scope enum. See §2.2. |
| `signal_name` | `String` | yes | Logical name (e.g. `"approval_result"`, `"webhook_callback"`, `"tool_output"`). Used for matching. |
| `signal_category` | `SignalCategory` | yes | Categorization enum. See §4. |
| `payload` | `Bytes` | no | Opaque signal data (approval decision, callback body, tool output, etc.). |
| `payload_encoding` | `String` | no | Encoding hint (e.g. `"json"`, `"protobuf"`). Default: `"json"`. |
| `source_type` | `SourceType` | yes | Who sent this signal: `external_api`, `webhook`, `operator`, `engine`, `worker`, `flow_coordinator`. |
| `source_identity` | `String` | yes | Identity of the sender (API key ID, webhook source, operator ID, worker ID). |
| `correlation_id` | `String` | no | Caller-supplied correlation for tracing across systems. |
| `idempotency_key` | `String` | no | Dedup key. If present, engine rejects duplicate signals with the same key for the same target. |
| `created_at` | `Timestamp` | yes | When the signal was submitted to the engine. |
| `accepted_at` | `Timestamp` | no | When the engine durably accepted the signal. May differ from `created_at` if queued. |
| `observed_effect` | `ObservedEffect` | yes | What the engine actually did with this signal. See §6. |
| `matched_waitpoint_id` | `UUID` | no | The waitpoint this signal was matched against (may differ from `target_waitpoint_id` if resolved from key). |

#### 2.2 Target Scope

| Value | Description |
|-------|-------------|
| `waitpoint` | Signal targets a specific waitpoint by ID or key. This is the primary and recommended scope for resume/data signals. |
| `execution` | Signal targets an execution directly. Valid only when the execution has exactly one open waitpoint (convenience for simple single-suspension patterns). Rejected if ambiguous. |

**Design note:** Generic execution-level signal buffering (without a known waitpoint) is intentionally not supported. It is too dangerous once retries, reclaims, replays, and multiple suspension episodes exist. Signals must target a known waitpoint.

---

### 3. Invariants

| ID | Invariant | Rationale |
|----|-----------|-----------|
| G1 | **Durable receipt.** Once accepted, a signal must be durably recorded before it is considered delivered. | Signals must survive engine restarts. A callback that vanishes is a correctness bug. |
| G2 | **Targeted delivery.** A signal targets a specific execution or waitpoint scope. It is not anonymous broadcast. | Prevents cross-execution contamination and stale-signal confusion. |
| G3 | **Delivery is separate from effect.** A signal being stored does not automatically mean the execution resumes. Resume depends on execution state and resume condition rules. | Decouples signal acceptance from state transition. Enables multi-signal conditions. |
| G4 | **Ordered visibility per execution.** Signals for one execution must have a stable per-target order for inspection and replay of control history. | Operators need to see "what signals arrived and in what order." |
| G5 | **Dedup support.** Signal delivery must support idempotency keys when external systems may retry callbacks. | Webhooks are retried. Duplicate delivery must not produce duplicate resume events. |

---

### 4. Signal Categories

Categories are not mandatory API nouns — they are metadata that helps operators reason about signals and enables category-based matching.

| Category | Description | Typical Source |
|----------|-------------|----------------|
| `approval` | Human or system approved a gated action. | Operator, external approval system |
| `rejection` | Human or system rejected a gated action. | Operator, external approval system |
| `callback` | External system delivered a callback result. | Webhook, external API |
| `tool_result` | External tool completed and returned output. | Tool execution system, worker |
| `timeout` | Synthetic signal generated by engine on suspension timeout. | Engine |
| `continue` | Step continuation data for resuming multi-step execution. | Worker, orchestrator |
| `custom` | Application-defined domain signal. | External API, domain system |

The `timeout` category is engine-generated. When a suspension timeout fires, the engine creates a synthetic timeout signal for auditability before applying the timeout policy. This ensures the audit trail always shows *why* a suspension ended.

---

### 5. Delivery Model

#### 5.1 Delivery Rules

| Rule | Description |
|------|-------------|
| **Deliver to waitpoint** | Primary path. Signal targets a specific `waitpoint_id` or is resolved from `waitpoint_key`. Signal is recorded against that waitpoint. Resume conditions are evaluated. |
| **Deliver to execution (convenience)** | Signal targets `execution_id` with `target_scope = execution`. Engine resolves to the single open waitpoint. If zero or multiple open waitpoints exist, signal is rejected with `ambiguous_target`. |
| **Reject if not signalable** | If the execution is `active`, `terminal`, or `runnable` (not suspended and no pending waitpoint), signal is rejected with `target_not_signalable`. |
| **Buffer for pending waitpoint** | If a waitpoint has been pre-created (pending, not yet committed to suspension) and the signal targets it by ID or key, the signal is accepted and buffered. When suspension commits, buffered signals are evaluated. See §5.3. |
| **Reject stale/closed waitpoint** | If the targeted waitpoint is already closed (satisfied or expired), signal is rejected with `waitpoint_closed` or recorded as `no_op` per policy. |

#### 5.2 What Signals Must NOT Do

- Signals must not cancel, complete, or fail an execution. Use explicit control operations.
- Signals must not revoke or modify a lease. Use `revoke_lease`.
- Signals must not change execution priority, routing, or policy. Use control operations.
- Signals delivered to an active (non-suspended) execution must be rejected unless a pending waitpoint exists.
- Signals must not be buffered generically by execution ID alone. That is too dangerous with retries, reclaims, replays, and multiple suspension episodes.

#### 5.3 Pending Waitpoint Buffering

To solve callback/approval races, the engine supports **pending waitpoints** (defined in RFC-004).

The race: a worker calls an external tool, then suspends. The tool result arrives *before* the suspension is committed. Without pending waitpoints, the signal would be rejected.

The solution:
1. Worker pre-creates a waitpoint (`waitpoint_id` + optional `waitpoint_key`) before suspension.
2. External system sends signal targeting the waitpoint by key.
3. Engine accepts the signal because the waitpoint exists (even though suspension is not yet committed).
4. When suspension commits, the waitpoint attaches to the suspension record. Buffered signals are evaluated against resume conditions.
5. If the worker never actually suspends (e.g. it completes synchronously), the pending waitpoint expires or is explicitly closed. Buffered signals are recorded as `no_op_waitpoint_never_activated`.

This is a **correctness feature**, not generic signal buffering. Only signals targeting a known pending waitpoint are buffered.

---

### 6. Effect Model

When a signal is delivered, the engine records the **actual effect** for auditability. The signal record's `observed_effect` field captures what happened.

| Effect | Description |
|--------|-------------|
| `no_op` | Signal was accepted and recorded but had no state effect. Waitpoint conditions not yet satisfied. |
| `no_op_waitpoint_never_activated` | Signal was buffered for a pending waitpoint that was never activated. |
| `buffered_for_pending_waitpoint` | Signal accepted against a pending (not yet committed) waitpoint. Will be evaluated on suspension commit. **Tentative** — if suspension never commits, the signal is discarded (see §12.3). Response includes `requires_confirmation = true`. |
| `appended_to_waitpoint` | Signal recorded against an open waitpoint. Resume conditions not yet fully satisfied. |
| `resume_condition_satisfied` | Signal satisfied the waitpoint's resume condition. Execution becomes eligible for `waiting` → claimable. |
| `duplicate_ignored` | Idempotency key matched a previously accepted signal. No new record created. |
| `rejected_target_not_signalable` | Target execution is not in a signalable state. |
| `rejected_waitpoint_closed` | Target waitpoint is already closed/satisfied. |
| `rejected_ambiguous_target` | Execution-scoped signal but multiple or zero open waitpoints exist. |

The engine must record effects **atomically** with the signal acceptance. An accepted signal with an unknown effect is a correctness bug — it means the engine accepted data but does not know what it did.

---

### 7. Signal Consumption Semantics

#### 7.1 Waitpoints Are Multi-Signal Until Closed

A waitpoint may accept multiple signals while open. This supports:
- Multi-condition waits (e.g. "approval from manager AND approval from compliance")
- Signal accumulation before a join/threshold is met
- Multiple tool results arriving for one suspension episode

#### 7.2 Idempotency

If a signal carries an `idempotency_key`, the engine checks for a prior signal with the same key targeting the same waitpoint:
- If found: the new signal is rejected with `duplicate_ignored`. No new record is created. The response returns the original `signal_id`.
- If not found: the signal is accepted normally.

Idempotency scope is `(target_waitpoint_id, idempotency_key)`. The same key on different waitpoints creates distinct signals.

#### 7.3 Waitpoint Closure

Once a waitpoint's resume condition is fully satisfied:
1. The waitpoint is marked **closed**.
2. The execution transitions from `suspended` to `runnable` (re-enters scheduling).
3. Later signals targeting this waitpoint are either:
   - Rejected with `waitpoint_closed` (default), or
   - Recorded as `no_op` with `observed_effect = rejected_waitpoint_closed` (for audit retention)

One-shot approval/callback UX is layered on top of this: a waitpoint with `signal_match_mode = single` closes after the first matching signal.

#### 7.4 Signal Payload Access on Resume

When a worker reclaims a resumed execution, it can read the signals that satisfied the waitpoint:
- Signal payloads are available via `list_waitpoint_signals(waitpoint_id)`
- The execution's continuation metadata (RFC-004) may reference which signal payloads to process
- Signal payloads are immutable after acceptance — the worker reads them, it does not modify them

---

### 8. Signal Matching Model

Signals are matched against waitpoint resume conditions. The matching system determines whether a delivered signal contributes to satisfying the condition.

#### 8.1 Match Criteria

A signal matches a waitpoint's resume condition if it satisfies all **required** match fields:

| Match Field | Description |
|-------------|-------------|
| `waitpoint_id` / `waitpoint_key` | Signal must target this specific waitpoint. Always required. |
| `signal_name` | If the resume condition specifies required signal names, the signal's name must be in the set. |
| `signal_category` | If the resume condition specifies required categories, the signal's category must match. |
| `correlation_id` | If the resume condition specifies a required correlation, the signal must carry it. |
| `source_type` | If the resume condition restricts source types, the signal's source must match. |

#### 8.2 Resume Condition Structure

Resume conditions are **defined by RFC-004** on the waitpoint/suspension and **evaluated by signal delivery** (this RFC). This RFC does not redefine them — it consumes them. The authoritative schema is in RFC-004 §Resume Condition Model:

- `condition_type`: `signal_set`, `single_signal`, `operator_only`, `timeout_only`, `external_predicate`
- `required_signal_names`: signal names that can satisfy the condition
- `signal_match_mode`: `any` or `all` (RFC-004 defines these; `count(n)` is a designed-for extension)
- `minimum_signal_count`: minimum matching signals before satisfaction
- `timeout_behavior`: `fail`, `cancel`, `expire`, `auto_resume_with_timeout_signal`, `escalate`
- `allow_operator_override`: whether privileged resume may bypass normal matching

The signal delivery engine reads these fields from the waitpoint's condition hash and evaluates incoming signals against them.

#### 8.3 Evaluation Algorithm

```
on signal_delivered(signal, waitpoint):
    for each matcher in waitpoint.required_signals:
        if signal matches matcher AND matcher not already satisfied:
            mark matcher as satisfied by signal.signal_id
            break  // one signal satisfies at most one matcher

    if waitpoint.signal_match_mode == any:
        satisfied = any matcher is satisfied
    elif waitpoint.signal_match_mode == all:
        satisfied = all matchers are satisfied
    elif waitpoint.signal_match_mode == count(n):
        satisfied = count(satisfied matchers) >= n

    if satisfied:
        close waitpoint
        transition execution: suspended → runnable
        return resume_condition_satisfied
    else:
        return appended_to_waitpoint
```

For v1, the primary path is `signal_match_mode = any` with a single matcher (one-shot approval/callback). The multi-signal machinery exists for correctness but complex multi-condition evaluation is a v1-should-have, not a must-have.

---

### 9. Interaction with Suspension (RFC-004)

Signal delivery is tightly coupled with the suspension/waitpoint model defined in RFC-004. Key interaction points:

| Concern | Signal RFC (this) | Suspension RFC (RFC-004) |
|---------|-------------------|--------------------------|
| **Waitpoint ownership** | Signals target waitpoints. | Suspension creates and owns waitpoints. |
| **Pending waitpoints** | Signals may be buffered for pending waitpoints. | Worker pre-creates pending waitpoints before suspension commit. |
| **Resume condition** | Signal matching evaluates resume conditions. | Suspension defines resume conditions on the waitpoint. |
| **State transition** | Signal delivery may trigger `suspended` → `runnable`. | Suspension defines what happens on resume (continuation metadata, next step). |
| **No lease during suspension** | Signals do not require lease validation (execution is unowned). | RFC-003 confirms: suspension releases ownership. |
| **Timeout** | Engine generates synthetic `timeout` signal. | Suspension defines timeout_behavior policy. |

**Critical rule:** Resume/data signals are scoped to executions and waitpoints. Passive flow containers do NOT own waitpoints. If flow-level orchestration needs to wait for signals, that behavior lives on a coordinator execution inside the flow, not on the flow container itself. This keeps one signal model, not two.

---

### 10. Operations

#### 10.1 Signal Delivery Operations (Class A — atomic)

| Operation | Parameters | Semantics |
|-----------|-----------|-----------|
| `send_signal` | `target_execution_id`, `signal_name`, `signal_category`, `payload?`, `payload_encoding?`, `source_type`, `source_identity`, `correlation_id?`, `idempotency_key?`, `target_waitpoint_key?` | Delivers a signal to an execution. If `target_waitpoint_key` is provided, the control plane decodes the self-routing opaque token (RFC-004) to extract partition + waitpoint_id — no lookup required. If no key, resolves to the single open waitpoint (rejects if ambiguous). Atomically: records signal, evaluates resume condition, optionally transitions execution state. Returns `signal_id` + `observed_effect`. |
| `send_signal_to_waitpoint` | `target_waitpoint_id`, `signal_name`, `signal_category`, `payload?`, `payload_encoding?`, `source_type`, `source_identity`, `correlation_id?`, `idempotency_key?` | Delivers a signal directly to a waitpoint by internal ID. The caller must know the execution_id (derived from the waitpoint record or passed alongside). Same atomic semantics as `send_signal`. |

Both operations are **Class A** because signal acceptance, resume condition evaluation, and potential execution state transition must be atomic. A signal that is recorded but whose resume effect is lost is a correctness bug.

#### 10.2 Inspection Operations (Class C — derived/best-effort)

| Operation | Parameters | Returns |
|-----------|-----------|---------|
| `get_signal` | `signal_id` | Full signal record. |
| `list_signals` | `target_execution_id`, `waitpoint_id?`, `category?`, `limit`, `cursor` | Paginated signal history for an execution or waitpoint. Ordered by `accepted_at`. |
| `list_waitpoint_signals` | `waitpoint_id` | All signals delivered to a specific waitpoint. Used by workers on resume to read signal payloads. |
| `get_pending_signals` | `execution_id` | Signals buffered for pending (not yet committed) waitpoints. |
| `evaluate_resume_conditions` | `execution_id` | Re-evaluates resume conditions against current signal state. Useful for operator diagnosis ("why hasn't this resumed yet?"). |

---

### 11. Error Model

| Error | When |
|-------|------|
| `target_not_found` | `target_execution_id` does not exist. |
| `waitpoint_not_found` | `target_waitpoint_id` or resolved `target_waitpoint_key` does not exist. |
| `target_not_signalable` | Execution is not suspended and has no pending waitpoint. Signal delivery to active/terminal/runnable executions is forbidden. |
| `ambiguous_target` | `target_scope = execution` but zero or multiple open waitpoints exist. Caller must specify `waitpoint_id` or `waitpoint_key`. |
| `waitpoint_closed` | Target waitpoint is already satisfied/closed. Signal arrives too late. |
| `duplicate_signal` | `idempotency_key` matches a previously accepted signal for the same waitpoint. Returns existing `signal_id`. |
| `invalid_signal_payload` | Payload fails validation (e.g. exceeds size limit, encoding mismatch). |
| `signal_rejected_by_policy` | Lane or execution policy forbids signals of this category or from this source. |
| `invalid_waitpoint_key` | `target_waitpoint_key` could not be decoded (invalid MAC, corrupted token, or expired nonce). |
| `payload_too_large` | Signal payload exceeds the 64KB v1 limit. Use reference-based delivery for large payloads. |
| `signal_limit_exceeded` | Per-execution signal count has reached `max_signals_per_execution` (default: 10,000). Protects against unbounded memory growth from webhook retry storms. The caller should stop retrying or use idempotency keys to avoid creating new signal records. |

---

### 12. Valkey Data Model

#### 12.1 Key Schema

All signal-related keys share the `{p:N}` partition tag of their target execution, enabling atomic Lua scripts for signal delivery + resume condition evaluation + state transition.

##### Signal record (hash per signal)

```
ff:signal:{p:N}:<signal_id>  →  HASH
  signal_id           → UUID
  target_execution_id → UUID
  target_waitpoint_id → UUID
  target_scope        → waitpoint | execution
  signal_name         → approval_result
  signal_category     → approval
  source_type         → external_api
  source_identity     → api-key-abc123
  correlation_id      → corr-xyz789
  idempotency_key     → idem-webhook-retry-3
  created_at          → 1713100800000
  accepted_at         → 1713100800005
  observed_effect     → resume_condition_satisfied
  matched_waitpoint_id → wp-uuid-456
  payload_encoding    → json
```

Signal payload is stored separately (max 64KB in v1) to avoid loading large blobs on reads:

```
ff:signal:{p:N}:<signal_id>:payload  →  raw bytes (max 64KB)
```

##### Per-waitpoint signal history (Valkey Stream)

```
ff:wp:{p:N}:<waitpoint_id>:signals  →  STREAM
  Each entry (via XADD):
    signal_id        → UUID
    signal_name      → approval_result
    signal_category  → approval
    source_type      → external_api
    source_identity  → api-key-abc123
    matched          → 1
    accepted_at      → 1713100800005
```

Valkey Stream provides append-only ordered history with native XRANGE/XREAD support. Used by `list_waitpoint_signals` and for audit. Trimmed with `MAXLEN ~ <waitpoint_signal_maxlen>`.

##### Per-execution signal index (sorted set)

```
ff:exec:{p:N}:<execution_id>:signals  →  ZSET
  member: signal_id
  score:  accepted_at (ms)
```

Cross-waitpoint signal history for the execution. Used by `list_signals` and operator inspection.

##### Waitpoint key routing (no stored index)

**No `ff:wpkey` resolution index exists.** Per RFC-004, the `waitpoint_key` is a self-routing opaque token that encodes `partition_number`, `execution_id`, `waitpoint_id`, nonce, and MAC. Signal ingress decodes the token to extract the partition and route directly to the correct `{p:N}` keys. This avoids a cross-partition lookup entirely.

##### Idempotency index (per waitpoint)

```
ff:sigdedup:{p:N}:<waitpoint_id>:<idempotency_key>  →  STRING
  value: signal_id
  TTL:   signal_dedup_window (e.g. 24h)
```

`SET NX` ensures only one signal per idempotency key per waitpoint.

##### Waitpoint resume condition state (hash)

```
ff:wp:{p:N}:<waitpoint_id>:condition  →  HASH
  condition_type     → signal_set | single_signal | operator_only | timeout_only
  match_mode         → any | all
  total_matchers     → 2
  minimum_signal_count → 1
  satisfied_count    → 1
  matcher:0:name     → manager_approval
  matcher:0:satisfied → 1
  matcher:0:signal_id → sig-uuid-123
  matcher:1:name     → compliance_approval
  matcher:1:satisfied → 0
  matcher:1:signal_id →
  closed             → 0
  closed_at          →
  closed_reason      →
```

This hash tracks which matchers have been satisfied and by which signal. Schema follows RFC-004's resume condition model. The hash is the authoritative evaluation state; the Valkey Stream above is the audit trail.

#### 12.2 Atomic Signal Delivery Script

`deliver_signal.lua`

This script atomically: validates target, checks idempotency, records signal, evaluates resume condition, and on satisfaction: closes waitpoint + suspension records, transitions execution state, clears suspension fields on execution core, and respects resume_delay_ms.

**Critical design note:** Resume after suspension continues the **same attempt** — it does NOT create a new attempt. The `attempt_state` returns to `running_attempt` when a worker re-claims the resumed execution. Here, on signal-triggered resume, we set `attempt_state = attempt_interrupted` (paused, awaiting re-claim).

```lua
-- KEYS: exec_core, wp_condition, wp_signals_stream,
--       exec_signals_zset, signal_hash, signal_payload,
--       idem_key, waitpoint_hash, suspension_current,
--       eligible_zset, suspended_zset, delayed_zset,
--       suspension_timeout_zset
-- ARGV: signal fields, now_ms, signal_id, payload, idempotency_key,
--       waitpoint_id, execution_id, dedup_ttl_ms, resume_delay_ms,
--       signal_maxlen

local now_ms = tonumber(ARGV.now_ms)

-- 1. Validate execution state
local core = redis.call("HGETALL", KEYS[1])
if not core.id then return err("target_not_found") end

local lp = core.lifecycle_phase
if lp == "active" or lp == "terminal" or lp == "submitted" then
  -- Safety check: execution is not suspended. Check for pending waitpoint.
  -- NOTE: If waitpoint state=pending, the engine should have dispatched to
  -- buffer_signal_for_pending_waitpoint (#18), not this script.
  -- This check catches misrouted calls.
  local wp = redis.call("HGETALL", KEYS[8])
  if not wp.state or wp.state == "" then
    return err("target_not_signalable")
  end
  if wp.state == "pending" then
    return err("waitpoint_pending_use_buffer_script")
  end
  if wp.state ~= "active" then
    return err("target_not_signalable")
  end
  -- Active waitpoint on non-suspended execution — unusual but valid (race window)
end

-- 2. Validate waitpoint is open
local wp_cond = redis.call("HGETALL", KEYS[2])
if not wp_cond.condition_type then
  return err("waitpoint_not_found")
end
if wp_cond.closed == "1" then
  return err("waitpoint_closed")
end

-- 2b. Signal count limit (prevents unbounded ZSET growth from webhook retry storms)
local max_signals = tonumber(ARGV.max_signals_per_execution or "10000")
if max_signals > 0 then
  local current_count = redis.call("ZCARD", KEYS[4])
  if current_count >= max_signals then
    return err("signal_limit_exceeded")
  end
end

-- 3. Check idempotency
if ARGV.idempotency_key ~= "" then
  local existing = redis.call("GET", KEYS[7])
  if existing then
    return ok_duplicate(existing)  -- return existing signal_id
  end
  redis.call("SET", KEYS[7], ARGV.signal_id,
    "PX", tonumber(ARGV.dedup_ttl_ms), "NX")
end

-- 4. Record signal hash
redis.call("HSET", KEYS[5],
  "signal_id", ARGV.signal_id,
  "target_execution_id", ARGV.execution_id,
  "target_waitpoint_id", ARGV.waitpoint_id,
  "target_scope", ARGV.target_scope,
  "signal_name", ARGV.signal_name,
  "signal_category", ARGV.signal_category,
  "source_type", ARGV.source_type,
  "source_identity", ARGV.source_identity,
  "correlation_id", ARGV.correlation_id,
  "idempotency_key", ARGV.idempotency_key,
  "created_at", ARGV.created_at,
  "accepted_at", now_ms,
  "matched_waitpoint_id", ARGV.waitpoint_id,
  "payload_encoding", ARGV.payload_encoding)
if ARGV.payload ~= "" then
  redis.call("SET", KEYS[6], ARGV.payload)
end

-- 5. Append to signal history (Valkey Stream) + execution index
redis.call("XADD", KEYS[3], "MAXLEN", "~",
  tonumber(ARGV.signal_maxlen), "*",
  "signal_id", ARGV.signal_id,
  "signal_name", ARGV.signal_name,
  "signal_category", ARGV.signal_category,
  "source_type", ARGV.source_type,
  "source_identity", ARGV.source_identity,
  "matched", "0",
  "accepted_at", now_ms)
redis.call("ZADD", KEYS[4], now_ms, ARGV.signal_id)

-- 6. Evaluate resume condition
local effect = "appended_to_waitpoint"
local matched = false

local total = tonumber(wp_cond.total_matchers)
for i = 0, total - 1 do
  local sat_key = "matcher:" .. i .. ":satisfied"
  local name_key = "matcher:" .. i .. ":name"
  if wp_cond[sat_key] == "0" then
    local matcher_name = wp_cond[name_key]
    if matcher_name == "" or matcher_name == ARGV.signal_name then
      redis.call("HSET", KEYS[2],
        sat_key, "1",
        "matcher:" .. i .. ":signal_id", ARGV.signal_id)
      matched = true
      local new_sat = tonumber(wp_cond.satisfied_count) + 1
      redis.call("HSET", KEYS[2], "satisfied_count", new_sat)

      -- Update matched flag in signal stream entry
      -- (best-effort — stream entry is immutable, but we track in condition hash)

      local mode = wp_cond.match_mode
      local min_count = tonumber(wp_cond.minimum_signal_count or "1")
      local resume = false
      if mode == "any" then
        resume = (new_sat >= min_count)
      elseif mode == "all" then
        resume = (new_sat >= total)
      end

      if resume then
        effect = "resume_condition_satisfied"

        -- OOM-SAFE WRITE ORDERING (per §4.8b defensive write ordering):
        -- exec_core HSET is the "point of no return" — write it FIRST.
        -- If OOM kills the script after exec_core but before closing
        -- sub-objects, the execution is runnable (correct) with stale
        -- suspension/waitpoint records (reconciler catches this).
        -- If we closed sub-objects first and OOM hit before exec_core,
        -- the execution stays suspended with closed sub-objects — zombie.

        -- 7a. Transition execution: suspended → runnable (WRITE FIRST)
        -- Resume continues the SAME attempt (no new attempt created)
        if lp == "suspended" then
          local resume_delay = tonumber(ARGV.resume_delay_ms)
          local es, br, bd, ps, as_val
          if resume_delay > 0 then
            es = "not_eligible_until_time"
            br = "waiting_for_resume_delay"
            bd = "resume delay " .. resume_delay .. "ms after signal " .. ARGV.signal_name
            ps = "delayed"
          else
            es = "eligible_now"
            br = "waiting_for_worker"
            bd = ""
            ps = "waiting"
          end
          -- attempt_state = attempt_interrupted (paused, awaiting re-claim)
          -- same attempt continues — NOT pending_retry_attempt
          as_val = "attempt_interrupted"

          -- ALL 7 state vector dimensions (long canonical names)
          redis.call("HSET", KEYS[1],
            "lifecycle_phase", "runnable",
            "ownership_state", "unowned",
            "eligibility_state", es,
            "blocking_reason", br,
            "blocking_detail", bd,
            "terminal_outcome", "none",
            "attempt_state", as_val,
            "public_state", ps,
            "current_suspension_id", "",
            "current_waitpoint_id", "",
            "last_transition_at", now_ms, "last_mutation_at", now_ms)

          -- 7b. Update scheduling indexes
          local priority = tonumber(core.priority or "0")
          local created_at = tonumber(core.created_at or "0")
          redis.call("ZREM", KEYS[11], ARGV.execution_id)
          if resume_delay > 0 then
            redis.call("ZADD", KEYS[12],
              now_ms + resume_delay, ARGV.execution_id)
          else
            redis.call("ZADD", KEYS[10],
              0 - (priority * 1000000000000) + created_at,
              ARGV.execution_id)
          end
        end

        -- 7c. Close waitpoint condition (after exec_core is safe)
        redis.call("HSET", KEYS[2],
          "closed", "1", "closed_at", now_ms,
          "closed_reason", "satisfied")

        -- 7d. Close waitpoint record
        redis.call("HSET", KEYS[8],
          "state", "closed",
          "satisfied_at", now_ms,
          "closed_at", now_ms,
          "close_reason", "resumed")

        -- 7e. Close suspension record
        redis.call("HSET", KEYS[9],
          "satisfied_at", now_ms,
          "closed_at", now_ms,
          "close_reason", "resumed")

        -- 7f. Remove from suspension timeout index
        redis.call("ZREM", KEYS[13], ARGV.execution_id)
      end
      break
    end
  end
end

if not matched then
  effect = "no_op"
end

-- 8. Record observed effect on signal
redis.call("HSET", KEYS[5], "observed_effect", effect)

return ok(ARGV.signal_id, effect)
```

#### 12.3 Pending Waitpoint Buffer Script

`buffer_signal_for_pending_waitpoint.lua`

A simpler variant used when the waitpoint exists but suspension is not yet committed:

```lua
-- Same validation as deliver_signal but:
-- - Does NOT evaluate resume conditions (suspension not yet committed)
-- - Sets observed_effect = "buffered_for_pending_waitpoint"
-- - On suspension commit, a separate script replays buffered signals
--   through the full evaluation path
```

**Important: `buffered_for_pending_waitpoint` is a tentative acknowledgment.** The signal is durably recorded against the pending waitpoint, but the waitpoint may never be activated (worker crashes before calling `suspend_execution`). In that case, the pending waitpoint expiry scanner (RFC-004 §Pending waitpoint expiry scanner) closes the waitpoint with `close_reason = never_committed` and the signal's `observed_effect` is updated to `no_op_waitpoint_never_activated`. The signal data is then cleaned up.

**Caller responsibility:** External systems that receive `observed_effect = buffered_for_pending_waitpoint` in the response MUST implement at-least-once delivery with a confirmation timeout. Specifically:
1. After receiving `buffered_for_pending_waitpoint`, start a confirmation timer (recommended: 2× the expected suspension commit time, e.g., 30-60 seconds).
2. If the execution does not transition to `suspended` or `runnable` within the timer, re-send the signal.
3. On re-send, use the same `idempotency_key` — if the waitpoint was activated and the original signal was already evaluated, the re-send is deduplicated harmlessly.

The SDK's `send_signal` response should include a `requires_confirmation` flag set to `true` when `observed_effect = buffered_for_pending_waitpoint`, signaling the caller that the delivery is tentative and may require retry. This flag is `false` for all other effects.

#### 12.4 Waitpoint Closure on Timeout Script

`timeout_waitpoint.lua`

Engine-initiated when suspension timeout fires:

```lua
-- 1. Verify waitpoint is still open
-- 2. Generate synthetic timeout signal (signal_category = timeout)
-- 3. Record signal with observed_effect based on timeout_behavior:
--    - fail: close waitpoint + suspension, set execution
--            terminal_outcome = failed, attempt_state = attempt_terminal
--    - cancel: close waitpoint + suspension, set execution
--              terminal_outcome = cancelled
--    - expire: close waitpoint + suspension, set execution
--              terminal_outcome = expired
--    - auto_resume: close waitpoint + suspension, transition
--                   execution to runnable (same as normal resume path)
--    - escalate: close current waitpoint, mutate suspension to
--                reason_code = waiting_for_operator_review with
--                operator-only resume condition (execution stays suspended)
-- 4. Clear current_suspension_id / current_waitpoint_id on exec core
--    (unless escalate, which creates new waitpoint)
-- 5. Update all indexes atomically (remove from suspended, add to
--    terminal/eligible/delayed as appropriate)
```

#### 12.5 Retention

Signal records follow the execution's retention policy. When an execution is purged:
- Delete all `ff:signal:{p:N}:<signal_id>` hashes and payloads for signals in the execution's signal index
- Delete `ff:exec:{p:N}:<execution_id>:signals` sorted set
- Delete per-waitpoint signal streams (`ff:wp:{p:N}:<waitpoint_id>:signals`) and condition hashes (`ff:wp:{p:N}:<waitpoint_id>:condition`)
- Idempotency keys (`ff:sigdedup:`) expire via TTL and need no explicit cleanup

---

## Interactions with Other Primitives

| Primitive | RFC | Interaction |
|-----------|-----|-------------|
| **Execution** | RFC-001 | Signals target executions. Signal delivery may transition execution from `suspended` → `runnable`. Execution state vector dimensions (`lifecycle_phase`, `eligibility_state`, `blocking_reason`, `attempt_state`, `public_state`) are updated atomically with signal delivery. |
| **Attempt** | RFC-002 | Signal delivery does NOT create a new attempt. Resume after suspension continues the **same attempt** — the original attempt was paused (not terminal) when execution entered suspension. When a worker re-claims the resumed execution, it resumes the same attempt with the same `attempt_index` and continuation context. |
| **Lease** | RFC-003 | No lease exists during suspension (L5). Signal delivery does not require lease validation. Workers read signal payloads after re-acquiring a lease on the resumed execution. |
| **Suspension / Waitpoint** | RFC-004 | Suspension creates waitpoints. Signals are delivered to waitpoints. Resume conditions are defined on waitpoints and evaluated by signal delivery. Pending waitpoints enable pre-suspension buffering. |
| **Flow** | Later RFC | Passive flow containers do not own waitpoints. Flow-level signal coordination uses a coordinator execution inside the flow. |

---

## V1 Scope

### In V1

- Full signal object with all fields defined in §2.1
- Signal vs control boundary enforced (signals rejected on non-suspended/non-pending-waitpoint executions)
- Invariants G1–G5 enforced
- All 7 signal categories
- Waitpoint-scoped delivery (primary path)
- Execution-scoped delivery (convenience, single-waitpoint only)
- Pending waitpoint buffering for callback/approval races
- Idempotency via `SET NX` with TTL
- Single-signal resume conditions (`signal_match_mode = any` with one matcher)
- Atomic Lua delivery + evaluation + state transition
- Signal history per waitpoint and per execution
- Self-routing waitpoint key (decode+route, no stored index)
- 64KB max signal payload size (v1 default)
- `timeout` synthetic signal generation
- All inspection operations

### Designed-for but deferred

- Multi-signal resume conditions (`all_of`, `count(n)`) — machinery exists, complex evaluation deferred
- Signal routing to flow coordinator — v1 requires explicit execution targeting
- Signal payload schema validation — v1 treats payload as opaque bytes
- Signal TTL / expiry independent of waitpoint — v1 signals live as long as the execution
- Signal replay / re-evaluation after policy change
- Bulk signal delivery API

---

## Open Questions

**Resolved:** Q1 — Escalate timeout behavior. Aligned with RFC-004 Q2: mutate in place. `escalate` changes the suspension's `reason_code` to `waiting_for_operator_review` and replaces the resume condition with an operator-only condition. The waitpoint stays open; the condition is replaced. No new suspension episode created.

**Resolved:** Buffered signal replay order = Valkey Stream insertion order (`accepted_at`). RFC-004's buffered signal evaluation code uses `XRANGE` which returns entries in insertion order. Engine acceptance order is authoritative.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 4 (Signal), Finalization §7 (waitpoint model), §8 (signal vs control boundary)
- RFC-001: Execution Object and State Model (worker-2)
- RFC-002: Attempt Model (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
- RFC-004: Suspension and Waitpoint Model (worker-1)
