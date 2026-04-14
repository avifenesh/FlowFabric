# RFC-004: Suspension and Waitpoint Model

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 3, Primitive 4 waitpoint lines, Finalization items 7 and 8

---

## Summary

This RFC defines FlowFabric's suspension model: the durable, intentional waiting state used when an execution pauses and must later continue after one or more resume conditions are satisfied. It defines the suspension object, waitpoint object, pending waitpoints, invariants, reason categories, resume conditions, timeout behavior, lifecycle, operations, and the Valkey-native atomicity model for suspend and resume. The core rule is simple: suspension is a first-class lifecycle state, not a vague blocked flag, and entering suspension atomically releases active ownership while preserving durable continuation context.

## Motivation

This RFC is driven by the interruptible execution use cases in the pre-RFC:

- UC-19 suspend awaiting signal
- UC-20 resume from external callback
- UC-21 human approval gate
- UC-22 wait for multiple conditions
- UC-23 step continuation
- UC-24 pause on policy or budget breach
- UC-25 external tool wait
- UC-26 preemption / handoff

Suspension is the primitive that distinguishes controlled waiting from failure, abandonment, or retry. Without it, long-running AI and workflow executions either hold leases forever, lose context, or force users to simulate waiting with ad hoc queues and control flags.

## Detailed Design

### Object Definition

An execution may have zero or one active suspension at a time, and may accumulate many suspension episodes over its full lifetime.

Suspension and waitpoint are separate but tightly related:

- a **suspension** is the execution-level waiting episode
- a **waitpoint** is the durable correlation point that signals target

One active suspension owns one active waitpoint. A pending waitpoint may exist briefly before the suspension commits.

#### Suspension object

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `suspension_id` | UUID | yes | Unique per suspension episode. New for every intentional suspension. |
| `execution_id` | UUID | yes | Stable parent execution identity. |
| `attempt_index` | u32 | yes | Zero-based current attempt index when suspension was entered. Suspension does not create a new execution identity. |
| `waitpoint_id` | UUID | yes | Internal durable identity of the current waitpoint. |
| `waitpoint_key` | opaque string | yes | External opaque handle for callback/approval delivery. Externally opaque, internally routable. |
| `reason_code` | enum | yes | One of the suspension reason categories defined in this RFC. |
| `requested_by` | enum | yes | `worker`, `operator`, `policy`, or `system_timeout_policy`. |
| `created_at` | unix ms | yes | Suspension creation time, from Valkey server time. |
| `timeout_at` | nullable unix ms | no | Deadline for timeout handling. |
| `resume_condition` | structured object | yes | Declarative rule that determines when suspension is satisfied. |
| `resume_policy` | structured object | yes | Policy for how satisfied suspension returns to scheduling and how signals are consumed/retained. |
| `continuation_metadata_pointer` | nullable string | no | Pointer to durable runtime continuation/checkpoint state. The data behind the pointer is intentionally worker-owned, not engine-owned. |
| `buffered_signal_summary` | structured object | yes | Compact summary of accepted signals for fast inspection and condition evaluation. |
| `last_signal_at` | nullable unix ms | no | Timestamp of the latest accepted signal for this suspension. |
| `satisfied_at` | nullable unix ms | no | When the resume condition became satisfied. |
| `closed_at` | nullable unix ms | no | When the suspension episode closed by resume, cancel, or timeout behavior. |
| `close_reason` | nullable string | no | `resumed`, `timed_out_fail`, `timed_out_cancel`, `timed_out_expire`, `timed_out_auto_resume`, `timed_out_escalate`, `cancelled`, `operator_resume`, `manual_close`. |

#### Waitpoint object

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `waitpoint_id` | UUID | yes | Internal durable identity of one waiting episode. |
| `execution_id` | UUID | yes | Parent execution identity. |
| `attempt_index` | u32 | yes | Attempt index the waitpoint belongs to. |
| `suspension_id` | nullable UUID | no | Null while pending, set when the suspension commits. |
| `waitpoint_key` | opaque string | yes | External opaque handle used for signal delivery. |
| `state` | enum | yes | `pending`, `active`, `satisfied`, `closed`, `expired`. |
| `created_at` | unix ms | yes | Creation time. |
| `activated_at` | nullable unix ms | no | Set when pending waitpoint becomes attached to a suspension. |
| `satisfied_at` | nullable unix ms | no | Set when resume condition becomes satisfied. |
| `closed_at` | nullable unix ms | no | Set when the waitpoint is no longer open. |
| `expires_at` | nullable unix ms | no | Pending-waitpoint expiry or active waitpoint timeout, depending on state. |
| `close_reason` | nullable string | no | `resumed`, `expired`, `cancelled`, `never_committed`, `replaced`, `manual_close`. |
| `signal_count` | u32 | yes | Count of accepted signals recorded for this waitpoint. |
| `matched_signal_count` | u32 | yes | Count of signals that matched the resume condition. |
| `last_signal_at` | nullable unix ms | no | Timestamp of last accepted signal. |

#### Waitpoint key routing

`waitpoint_key` must be externally opaque but internally routable to the correct partition in Valkey Cluster.

**Token format:** base64url-encoded binary blob. No padding.

**Token structure:**

```
[version: 1 byte] [partition_number: 2 bytes] [execution_id: 16 bytes]
[waitpoint_id: 16 bytes] [nonce: 8 bytes] [HMAC: 32 bytes]
```

Total: 75 bytes raw → 100 characters base64url.

**MAC algorithm:** HMAC-SHA256. Input: all fields preceding the HMAC (version + partition + execution_id + waitpoint_id + nonce). Key: system-level deployment secret (see below).

**MAC secret:** A system-level secret configured at deployment. All engine instances share the same secret. The secret is NOT per-namespace or per-execution — it is a deployment-wide signing key.

**Decoding on signal ingress:**
1. base64url-decode the token.
2. Extract version byte. If unsupported version → `invalid_waitpoint_key`.
3. Verify HMAC-SHA256 against the **current** secret.
4. If verification fails, retry with the **previous** secret (dual-key rotation — see below).
5. If both fail → `invalid_waitpoint_key`.
6. Extract `partition_number`, `execution_id`, `waitpoint_id`.
7. Route to `{p:<partition_number>}` keys. No cross-partition lookup.

**MAC secret rotation — dual-key verification:**

The engine maintains two secrets: `current_secret` and `previous_secret`. On rotation:
1. `previous_secret` ← `current_secret`
2. `current_secret` ← new secret
3. `previous_secret` is retained for a configurable grace period. Recommended: `max(suspension_timeout)` across all active suspensions, minimum 24 hours.
4. During the grace period, signal ingress verifies against `current_secret` first, then `previous_secret` on failure.
5. After the grace period, `previous_secret` is discarded. Outstanding tokens signed with the old secret become permanently invalid — their executions resume only via operator action or suspension timeout.
6. New waitpoint_keys are always signed with `current_secret`.

This ensures in-flight callbacks survive one secret rotation without disruption. Operators should drain active suspensions or extend the grace period before rotating a second time.

### Pending Waitpoints

Pending waitpoints solve the callback/approval race where an external system may deliver a signal before the worker has fully committed suspension.

Rules:

1. A waitpoint may be precreated while the execution is still active and lease-bearing.
2. A pending waitpoint is externally addressable by `waitpoint_key`.
3. Early signals may be accepted against the pending waitpoint and durably recorded.
4. When suspension commits, the pending waitpoint becomes `active` and is attached to the new suspension record.
5. If suspension never commits, the pending waitpoint must transition to `expired` or `closed` with `close_reason = never_committed`.
6. Pending waitpoints are a correctness feature, not generic speculative signal buffering.

### Invariants

#### S1. Intentional entry

Suspension must be entered explicitly, not inferred from crash, timeout, or worker disappearance.

Rationale:
- crash recovery is reclaim, not suspension
- operators must be able to distinguish intentional waiting from broken liveness

#### S2. No active ownership while suspended

A suspended execution must not retain a valid active lease.

Rationale:
- suspended work is not actively running
- holding ownership during waiting would block reclaim, reroute, drain, and explainability

#### S3. Durable wait condition

The reason, condition, timeout, and continuation context for suspension must be durably persisted and inspectable.

Rationale:
- resume semantics must survive process crashes and restarts
- "what is this waiting for?" must always be answerable

#### S4. Resume is explicit

A suspended execution resumes only because a recorded cause satisfied its resume condition:
- a matching signal
- an operator action
- a timeout policy
- a defined engine rule

Rationale:
- suspended work must not spontaneously wake up without audit evidence
- implicit wakeups make debugging and safety review impossible

#### S5. Signals survive the wait

Signals accepted for a waitpoint must be durably recorded according to delivery policy before they are considered observed.

Rationale:
- callbacks, approvals, and tool results are retried in the real world
- correctness requires that accepted signals survive worker crashes and control-plane restarts

### Suspension Reason Categories

Initial reason codes:

- `waiting_for_signal`
- `waiting_for_approval`
- `waiting_for_callback`
- `waiting_for_tool_result`
- `waiting_for_operator_review`
- `paused_by_policy`
- `paused_by_budget`
- `step_boundary`
- `manual_pause`

These reason codes map to execution `blocking_reason` values via `map_reason_to_blocking`:

| Suspension `reason_code` | RFC-001 `blocking_reason` |
|---|---|
| `waiting_for_signal` | `waiting_for_signal` |
| `waiting_for_approval` | `waiting_for_approval` |
| `waiting_for_callback` | `waiting_for_callback` |
| `waiting_for_tool_result` | `waiting_for_tool_result` |
| `waiting_for_operator_review` | `paused_by_operator` |
| `paused_by_policy` | `paused_by_policy` |
| `paused_by_budget` | `waiting_for_budget` |
| `step_boundary` | `waiting_for_signal` |
| `manual_pause` | `paused_by_operator` |

This mapping is implemented as a lookup table in the `ff_suspend_execution` function.

### Resume Condition Model

Resume conditions are declarative and durable. They define what must happen for the execution to leave suspension.

#### Resume condition fields

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `condition_type` | enum | yes | `signal_set`, `single_signal`, `operator_only`, `timeout_only`, `external_predicate`. |
| `required_signal_names` | list<string> | no | Signal names that can satisfy the condition. Empty for operator-only or timeout-only conditions. |
| `required_waitpoint_id` | UUID | yes | Internal waitpoint correlation target. |
| `required_waitpoint_key` | opaque string | no | External handle when needed for external systems. |
| `signal_match_mode` | enum | yes | `any`, `all`, or designed-for `count(n)`. V1 does not require ordered matching. |
| `minimum_signal_count` | u32 | no | Minimum number of accepted matching signals before satisfaction. |
| `timeout_behavior` | enum | yes | One of the timeout behaviors in this RFC. |
| `allow_operator_override` | bool | yes | Whether privileged operator resume may bypass normal signal matching. |

#### Resume policy fields

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `resume_target` | enum | yes | `runnable`. Resume returns to scheduling; it does not reacquire a lease inline. |
| `close_waitpoint_on_resume` | bool | yes | Normally true. |
| `consume_matched_signals` | bool | yes | Whether matched signals are considered consumed after satisfaction. |
| `retain_signal_buffer_until_closed` | bool | yes | Whether buffered signals remain readable until the waitpoint fully closes. |
| `resume_delay_ms` | nullable u64 | no | Optional delay before the resumed execution becomes eligible. |

### Timeout Behavior

When `timeout_at` is reached before the resume condition is satisfied, one of the following behaviors applies:

#### `fail`

- transition execution to terminal failed
- record `failure_reason = suspension_timeout`
- close waitpoint and suspension

#### `cancel`

- transition execution to terminal cancelled
- record cancellation reason
- close waitpoint and suspension

#### `expire`

- transition execution to terminal expired
- close waitpoint and suspension

#### `auto_resume_with_timeout_signal`

- append a synthetic `timeout` signal to the waitpoint buffer
- mark the resume condition satisfied
- close the current waitpoint
- transition execution back to `runnable`

#### `escalate`

- close the current waitpoint with `close_reason = timed_out_escalate`
- keep the execution non-terminal
- transition the execution into a new operator-review suspension episode or mutate the current suspension to `reason_code = waiting_for_operator_review` with an operator-only resume condition

Model note:

- all five behaviors are part of the object model
- v1 may implement a smaller subset first, but the durable schema must leave room for all five

### Waitpoint Lifecycle

Waitpoint states:

- `pending`
- `active`
- `satisfied`
- `closed`
- `expired`

Lifecycle rules:

1. `pending` is used only for precreated waitpoints before suspension commit.
2. `active` means the waitpoint is attached to the current suspension and open for signal matching.
3. `satisfied` means the resume condition has been met.
4. `closed` means the waitpoint completed its job by resume, cancel, replacement, or manual close.
5. `expired` means the waitpoint aged out before use or timed out without a successful resume path.

A waitpoint may accept multiple signals while it is `pending` or `active`. Once it is `closed` or `expired`, later signals are rejected or recorded as no-op by policy.

### Suspension Lifecycle

#### Enter suspension

Preconditions:

- execution is `active`
- caller holds a valid lease
- suspension spec is valid

Effects:

- create or attach a waitpoint
- persist suspension metadata
- release the lease atomically
- remove execution from active indexes
- set execution state to:
  - `lifecycle_phase = suspended`
  - `ownership_state = unowned`
  - `eligibility_state = not_applicable`
  - `blocking_reason = <reason_code-mapped blocking reason>`
- move the current attempt out of running state using RFC-002's suspend-path interruption / pause semantics

#### Receive signals

Signals target the current waitpoint by `waitpoint_id` or externally by `waitpoint_key`.

Effects:

- accepted signals are durably appended to the waitpoint signal buffer
- suspension summary fields are updated
- resume condition is re-evaluated

Signal boundary:

- signals are for approval results, callback payloads, tool results, and continue/resume inputs tied to a waitpoint
- active control actions such as cancel, revoke lease, reroute, and operator override are not modeled as generic signals

#### Resume eligibility

Once the resume condition is satisfied:

- the waitpoint becomes `satisfied`
- the suspension becomes closable
- the execution transitions back to `runnable`, not directly to `active`
- `eligibility_state` becomes:
  - `eligible_now` when ready immediately
  - `not_eligible_until_time` when `resume_delay_ms` is set
- `blocking_reason` becomes `waiting_for_worker` or `waiting_for_resume_delay` accordingly

Resume does not reacquire a lease inline. Claiming after resume uses the normal scheduler path.

#### Timeout / expiry

When `timeout_at` is reached before satisfaction:

- apply the configured timeout behavior atomically
- close or expire the waitpoint
- close the suspension episode
- update execution state accordingly

#### Operator intervention

Privileged operators may:

- resume immediately
- close the waitpoint
- cancel the suspension
- expire the suspension
- adjust timeout behavior or continuation metadata under auditable override

### Continuation Model

Suspension exists because execution needs to continue later with context. The engine must not own language-runtime continuations, but it must own enough durable metadata to restart meaningfully.

Required continuation support:

- `continuation_metadata_pointer` to runtime-owned durable state
- `last_completed_step`
- `resumable_cursor` or checkpoint reference
- access to buffered signal payloads on resume

Recommended shape:

- the engine stores a pointer or reference in the suspension record (via the `ff_suspend_execution` Valkey Function)
- the worker runtime (`ff-sdk`) owns the language-specific continuation material behind that pointer
- this split is intentional: the engine owns suspension correctness and durability boundaries, while `ff-sdk` owns language/runtime-specific continuation data
- resume hands the worker (`ff-sdk`):
  - execution identity
  - attempt index
  - continuation pointer
  - matched signals
  - recent buffered signal summary

Workers must not rely on re-reading their own attempt stream for continuation context across suspension. Stream frames may be trimmed by MAXLEN while the stream is open (RFC-006). All context needed for resumption must be stored behind the `continuation_metadata_pointer` or derived from matched signal payloads.

### Interaction with Execution State

Suspension is a lifecycle state, not merely a blocking reason.

Rules:

1. `active -> suspended` is a Class A atomic transition.
2. The transition must atomically:
   - validate the lease
   - release / clear ownership under RFC-003
   - create the suspension record
   - create or activate the waitpoint
   - update execution state
3. While suspended:
   - `ownership_state = unowned`
   - `eligibility_state = not_applicable`
   - `public_state = suspended`
4. Resume transitions to `runnable`, not `active`.
5. Terminal timeout behavior clears suspension and leaves no current waitpoint.

Attempt interaction:

- suspension does not create a new execution
- the same attempt continues across suspension and resume; suspension does not create a new attempt
- suspend transitions the current attempt record from `started -> suspended`
- resume plus later re-claim/claim transitions that same attempt from `suspended -> started`
- the execution state vector uses `attempt_state = attempt_interrupted` while suspended
- RFC-002 owns the exact attempt-record schema, but this RFC requires that no lease-bearing `started` attempt remain during suspension

### Operations

| Operation | Class | Semantics |
| --- | --- | --- |
| `suspend_execution(execution_id, attempt_index, lease_id, lease_epoch, suspension_spec)` | A | Validates the lease, releases ownership, creates the suspension, creates or activates the waitpoint, and moves execution to `suspended`. The current attempt record transitions to `suspended`. |
| `get_suspension(execution_id)` | C | Returns the current suspension record, if any. |
| `list_suspension_signals(execution_id, waitpoint_id?)` | C | Returns accepted signals for the current or specified waitpoint in stable order. |
| `resume_execution(execution_id, waitpoint_ref, trigger)` | A | Validates resume conditions or override rules, satisfies the waitpoint, closes the suspension, and moves execution to `runnable`. |
| `expire_suspension(execution_id)` | A | Applies timeout behavior to the active suspension. |
| `cancel_suspension(execution_id, reason)` | A | Closes the active suspension and transitions execution according to cancellation semantics. |
| `create_pending_waitpoint(execution_id, attempt_index, lease_id, lease_epoch, waitpoint_spec)` | A | Precreates an externally addressable pending waitpoint while the execution is still active. |
| `close_waitpoint(waitpoint_ref, reason)` | A | Closes a pending or active waitpoint under valid system/operator conditions. |

## Error Model

| Error | Meaning |
| --- | --- |
| `execution_not_active` | Tried to suspend an execution that is not currently active. |
| `invalid_lease_for_suspend` | Lease identity / epoch / attempt binding did not match current ownership. |
| `already_suspended` | Execution already has an active suspension. |
| `invalid_resume_condition` | Suspension spec contains an invalid or unsupported condition definition. |
| `execution_not_suspended` | Tried to resume, expire, or cancel when no active suspension exists. |
| `suspension_not_found` | Requested suspension record does not exist. |
| `waitpoint_not_found` | Waitpoint ID or key does not resolve to a known waitpoint. |
| `waitpoint_not_active` | Waitpoint exists but is not open for signal matching or resume. |
| `waitpoint_closed` | Waitpoint was already closed or expired. |
| `pending_waitpoint_expired` | A pending waitpoint aged out before suspension committed. |
| `resume_condition_not_met` | Resume was attempted before the condition was satisfied and no override applied. |
| `suspension_timeout_elapsed` | Timeout handling was attempted after timeout had already been resolved. |
| `invalid_waitpoint_for_execution` | Waitpoint does not belong to the specified execution or attempt index. |

## Valkey Data Model

### Partitioning and key conventions

This RFC uses the same partition-tag model as RFC-003.

For an execution:

- `partition_number = hash(execution_id) % num_partitions`
- partition is computed once at execution creation
- execution, lease, attempt, suspension, waitpoint, and local indexes use the same `{p:N}` hash tag

### Key schema

Authoritative keys:

- `ff:exec:{p:N}:<execution_id>:core`
- `ff:exec:{p:N}:<execution_id>:suspension:current`
- `ff:wp:{p:N}:<waitpoint_id>`
- `ff:wp:{p:N}:<waitpoint_id>:signals`

Secondary indexes:

- `ff:idx:{p:N}:suspension_timeout`
- `ff:idx:{p:N}:pending_waitpoint_expiry`
- `ff:idx:{p:N}:lane:<lane>:active`
- `ff:idx:{p:N}:lane:<lane>:eligible`

Required for cleanup:

- `ff:exec:{p:N}:<execution_id>:waitpoints` — MANDATORY SET of all waitpoint_ids created for this execution. SADD'd on every `suspend_execution`. Used by the retention cleanup cascade to discover and DEL all waitpoint-related keys (hash, :signals stream, :condition hash). Without this set, old waitpoint records from prior suspension episodes are orphaned.

Optional history:

- `ff:exec:{p:N}:<execution_id>:suspensions`

### Suspension current record

`ff:exec:{p:N}:<execution_id>:suspension:current` stores:

- `suspension_id`
- `execution_id`
- `attempt_index`
- `waitpoint_id`
- `waitpoint_key`
- `reason_code`
- `requested_by`
- `created_at`
- `timeout_at`
- `resume_condition_json`
- `resume_policy_json`
- `continuation_metadata_pointer`
- `buffered_signal_summary_json`
- `last_signal_at`
- `satisfied_at`
- `closed_at`
- `close_reason`

TTL strategy:

- no TTL while active
- after closure, `PEXPIREAT` may be used for cleanup according to execution retention policy

### Waitpoint record

`ff:wp:{p:N}:<waitpoint_id>` stores:

- `waitpoint_id`
- `execution_id`
- `attempt_index`
- `suspension_id`
- `waitpoint_key`
- `state`
- `created_at`
- `activated_at`
- `satisfied_at`
- `closed_at`
- `expires_at`
- `close_reason`
- `signal_count`
- `matched_signal_count`
- `last_signal_at`

### Signal buffer per waitpoint

`ff:wp:{p:N}:<waitpoint_id>:signals` is an append-only stream of accepted signals for this waitpoint.

Recommended stream fields:

- `signal_id`
- `signal_name`
- `source_type`
- `source_identity`
- `payload_ref` or compact payload
- `matched`
- `accepted_at`

Recommended trim policy:

- `XADD ... MAXLEN ~ <waitpoint_signal_maxlen>`

The stream is the authoritative per-waitpoint signal history. `buffered_signal_summary_json` on the suspension record is a read-optimized summary, not the source of truth.

### Waitpoint key routing

Because `waitpoint_key` must route to the correct partition without a cross-slot lookup, `ff-core` should derive it from the partitioned waitpoint identity:

- encode `partition_number`, `waitpoint_id`, `execution_id`, and a MAC into the opaque external key
- when a signal arrives with `waitpoint_key`, the control plane decodes the partition and routes the operation to the matching `{p:N}` keys

No global `waitpoint_key -> waitpoint_id` key is required for correctness.

### Atomic suspend function

`ff_suspend_execution` — registered in the `flowfabric` library via `redis.register_function()`. Shared helpers (`is_set`, `err`, `ok`, `mark_expired`, `hgetall_to_table`, `map_reason_to_blocking`, `initialize_condition`, `evaluate_signal_against_condition`, `is_condition_satisfied`, `write_condition_hash`, `extract_field`) are library-local functions available to all registered functions.

Keys:

- execution core
- current attempt record
- current lease hash
- lease history stream
- lease expiry zset
- worker lease set
- suspension current hash
- waitpoint hash
- waitpoint signal stream
- suspension timeout zset
- pending waitpoint expiry zset
- lane active index
- lane suspended index
- waitpoint history set
- waitpoint condition hash
- attempt timeout zset

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.core)

assert_execution_exists(core)

if core.lifecycle_phase ~= "active" then
  return err("execution_not_active")
end
-- Full lease validation matching complete_or_fail template (RFC-003):
-- expiry check, revocation check, identity check, attempt binding check
if core.ownership_state == "lease_revoked" or is_set(core.lease_revoked_at) then
  return err("lease_revoked")
end
if tonumber(core.lease_expires_at) <= now_ms then
  mark_expired(core, now_ms)
  return err("lease_expired")
end
if tostring(core.current_attempt_index) ~= ARGV.attempt_index then
  return err("invalid_lease_for_suspend")
end
if core.current_attempt_id ~= ARGV.attempt_id then
  return err("invalid_lease_for_suspend")
end
if core.current_lease_id ~= ARGV.lease_id or tostring(core.current_lease_epoch) ~= ARGV.lease_epoch then
  return err("invalid_lease_for_suspend")
end
-- Check for existing suspension: reject if open, archive if closed
if redis.call("EXISTS", KEYS.suspension_current) == 1 then
  local closed = redis.call("HGET", KEYS.suspension_current, "closed_at")
  if closed == nil or closed == "" then
    return err("already_suspended")  -- open suspension exists, reject
  end
  -- Previous suspension is closed. Archive old waitpoint_id for cleanup,
  -- then DEL the stale record before creating a new one.
  local old_wp = redis.call("HGET", KEYS.suspension_current, "waitpoint_id")
  if old_wp and old_wp ~= "" then
    redis.call("SADD", KEYS.waitpoint_history, old_wp)
  end
  redis.call("DEL", KEYS.suspension_current)
end

local waitpoint_id = ARGV.waitpoint_id
local waitpoint_key = ARGV.waitpoint_key
if ARGV.use_pending_waitpoint == "1" then
  local wp = redis.call("HGETALL", KEYS.waitpoint)
  validate_pending_waitpoint(wp, ARGV.execution_id, ARGV.attempt_index, waitpoint_key, now_ms)
  redis.call("HSET", KEYS.waitpoint,
    "suspension_id", ARGV.suspension_id,
    "state", "active",
    "activated_at", now_ms,
    "expires_at", ARGV.timeout_at or ""
  )
  redis.call("ZREM", KEYS.pending_waitpoint_expiry, waitpoint_id)

  -- CRITICAL: Evaluate buffered signals that arrived while waitpoint was pending.
  -- If early signals already satisfy the resume condition, skip suspension entirely.
  local buffered = redis.call("XRANGE", KEYS.waitpoint_signals, "-", "+")
  if #buffered > 0 then
    -- Initialize condition state from resume_condition_json
    local wp_cond = initialize_condition(ARGV.resume_condition_json)
    for _, entry in ipairs(buffered) do
      local fields = entry[2]
      local sig_name = extract_field(fields, "signal_name")
      evaluate_signal_against_condition(wp_cond, sig_name, extract_field(fields, "signal_id"))
    end
    if is_condition_satisfied(wp_cond) then
      -- Resume condition already met by buffered signals — skip suspension.
      -- Close waitpoint as satisfied (never truly suspended).
      redis.call("HSET", KEYS.waitpoint,
        "state", "closed", "satisfied_at", now_ms,
        "closed_at", now_ms, "close_reason", "resumed")
      -- Write condition state as satisfied
      write_condition_hash(KEYS.wp_condition, wp_cond, now_ms)
      -- Do NOT release lease, do NOT change execution state.
      -- Return 'already_satisfied' so caller knows suspension was skipped.
      return ok_already_satisfied(ARGV.suspension_id, waitpoint_id, waitpoint_key)
    end
    -- Condition not yet satisfied — proceed with normal suspension.
    -- Write partial condition state (some matchers may be satisfied).
    write_condition_hash(KEYS.wp_condition, wp_cond, now_ms)
  end
else
  redis.call("HSET", KEYS.waitpoint,
    "waitpoint_id", waitpoint_id,
    "execution_id", ARGV.execution_id,
    "attempt_index", ARGV.attempt_index,
    "suspension_id", ARGV.suspension_id,
    "waitpoint_key", waitpoint_key,
    "state", "active",
    "created_at", now_ms,
    "activated_at", now_ms,
    "expires_at", ARGV.timeout_at or "",
    "signal_count", 0,
    "matched_signal_count", 0,
    "last_signal_at", ""
  )
  -- Initialize condition hash from resume condition spec.
  -- Without this, deliver_signal returns waitpoint_not_found (condition hash missing).
  local wp_cond = initialize_condition(ARGV.resume_condition_json)
  write_condition_hash(KEYS.wp_condition, wp_cond, now_ms)
end

-- Record waitpoint_id in mandatory history set (required for cleanup cascade)
redis.call("SADD", KEYS.waitpoint_history, waitpoint_id)

-- OOM-SAFE WRITE ORDERING (per RFC-010 §4.8b):
-- exec_core HSET is the "point of no return" — write it FIRST.
-- If OOM kills after exec_core but before creating sub-objects,
-- execution is suspended (correct lifecycle) with missing suspension/
-- waitpoint records. The suspension timeout scanner and generalized
-- index reconciler can detect and initiate recovery (expire or re-enable).
-- Reverse order would leave sub-objects (suspension, waitpoint) existing
-- while exec_core still says 'active' — an unrecoverable zombie state
-- because no scanner looks for orphaned suspension records on active executions.

-- Step 1: Transition exec_core (FIRST — point of no return, all 7 dims)
redis.call("HSET", KEYS.core,
  "lifecycle_phase", "suspended",
  "ownership_state", "unowned",
  "eligibility_state", "not_applicable",
  "blocking_reason", map_reason_to_blocking(ARGV.reason_code),
  "blocking_detail", "suspended: waitpoint " .. waitpoint_id .. " awaiting " .. ARGV.reason_code,
  "terminal_outcome", "none",
  "attempt_state", "attempt_interrupted",
  "public_state", "suspended",
  "current_lease_id", "",
  "current_worker_id", "",
  "current_worker_instance_id", "",
  "lease_expires_at", "",
  "lease_last_renewed_at", "",
  "current_suspension_id", ARGV.suspension_id,
  "current_waitpoint_id", waitpoint_id,
  "last_transition_at", now_ms, "last_mutation_at", now_ms
)

-- Step 2: Pause the attempt: started -> suspended (RFC-002 suspend_attempt)
redis.call("HSET", KEYS.current_attempt,
  "attempt_state", "suspended",
  "suspended_at", now_ms,
  "suspension_id", ARGV.suspension_id)

-- Step 3: Release lease + update indexes (exec_core already correct)
redis.call("DEL", KEYS.current_lease)
redis.call("ZREM", KEYS.lease_expiry, ARGV.execution_id)
redis.call("SREM", KEYS.worker_leases, ARGV.execution_id)
redis.call("ZREM", KEYS.active_index, ARGV.execution_id)
redis.call("ZREM", KEYS.attempt_timeout_zset, ARGV.execution_id)  -- clear per-attempt timeout (§6.16)
redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
  "event", "released",
  "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch,
  "attempt_index", ARGV.attempt_index,
  "attempt_id", core.current_attempt_id,
  "reason", "suspend",
  "ts", now_ms
)

-- Step 4: Create suspension record (safe to lose on OOM — stale but not zombie)
redis.call("HSET", KEYS.suspension_current,
  "suspension_id", ARGV.suspension_id,
  "execution_id", ARGV.execution_id,
  "attempt_index", ARGV.attempt_index,
  "waitpoint_id", waitpoint_id,
  "waitpoint_key", waitpoint_key,
  "reason_code", ARGV.reason_code,
  "requested_by", ARGV.requested_by,
  "created_at", now_ms,
  "timeout_at", ARGV.timeout_at or "",
  "resume_condition_json", ARGV.resume_condition_json,
  "resume_policy_json", ARGV.resume_policy_json,
  "continuation_metadata_pointer", ARGV.continuation_metadata_pointer or "",
  "buffered_signal_summary_json", initial_signal_summary_json(),
  "last_signal_at", "",
  "satisfied_at", "",
  "closed_at", "",
  "close_reason", ""
)

-- Step 5: Add to per-lane suspended index + suspension timeout
redis.call("ZADD", KEYS.suspended_index,
  ARGV.timeout_at ~= "" and ARGV.timeout_at or 9999999999999,
  ARGV.execution_id)

if ARGV.timeout_at ~= "" then
  redis.call("ZADD", KEYS.suspension_timeout, ARGV.timeout_at, ARGV.execution_id)
end

return ok(ARGV.suspension_id, waitpoint_id, waitpoint_key)
```

### Atomic resume function

`ff_resume_execution`

This script is called after a matching signal or explicit operator resume trigger has already been durably accepted.

Keys:

- execution core
- suspension current hash
- waitpoint hash
- waitpoint signal stream
- suspension timeout zset
- lane eligible index
- lane delayed / not-eligible scheduling index

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.core)
local suspension = redis.call("HGETALL", KEYS.suspension_current)
local waitpoint = redis.call("HGETALL", KEYS.waitpoint)

if core.lifecycle_phase ~= "suspended" then
  return err("execution_not_suspended")
end
assert_active_suspension(suspension)
assert_waitpoint_belongs(waitpoint, ARGV.execution_id, suspension.suspension_id, suspension.waitpoint_id)

if waitpoint.state ~= "active" and waitpoint.state ~= "satisfied" then
  return err("waitpoint_not_active")
end
if not trigger_satisfies_resume_condition(suspension, waitpoint, ARGV.trigger_json, now_ms) then
  return err("resume_condition_not_met")
end

local eligibility_state = "eligible_now"
local blocking_reason = "waiting_for_worker"
local blocking_detail = ""
local public_state = "waiting"
local resume_delay_ms = resolve_resume_delay_ms(suspension.resume_policy_json)
if resume_delay_ms > 0 then
  eligibility_state = "not_eligible_until_time"
  blocking_reason = "waiting_for_resume_delay"
  blocking_detail = "resume delay " .. resume_delay_ms .. "ms"
  public_state = "delayed"
end

-- OOM-SAFE WRITE ORDERING (per RFC-010 §4.8b):
-- exec_core HSET is the "point of no return" — write it FIRST.
-- If OOM kills after exec_core but before closing sub-objects,
-- execution is runnable (correct) with stale suspension/waitpoint
-- records (generalized index reconciler catches this).
-- Reverse order would leave execution suspended with closed sub-objects — zombie.

-- Step 1: Transition exec_core (FIRST — point of no return)
redis.call("HSET", KEYS.core,
  "lifecycle_phase", "runnable",
  "ownership_state", "unowned",
  "eligibility_state", eligibility_state,
  "blocking_reason", blocking_reason,
  "blocking_detail", blocking_detail,
  "terminal_outcome", "none",
  "attempt_state", "attempt_interrupted",
  "public_state", public_state,
  "current_suspension_id", "",
  "current_waitpoint_id", "",
  "last_transition_at", now_ms, "last_mutation_at", now_ms
)

-- Step 2: Update scheduling indexes (exec_core already correct)
redis.call("ZREM", KEYS.suspended_index, ARGV.execution_id)
if eligibility_state == "eligible_now" then
  redis.call("ZADD", KEYS.eligible_index, now_ms, ARGV.execution_id)
else
  redis.call("ZADD", KEYS.delayed_index, now_ms + resume_delay_ms, ARGV.execution_id)
end

-- Step 3: Close sub-objects (safe to lose on OOM — stale but not zombie)
redis.call("HSET", KEYS.waitpoint,
  "state", "satisfied",
  "satisfied_at", now_ms
)
redis.call("HSET", KEYS.suspension_current,
  "satisfied_at", now_ms
)
redis.call("HSET", KEYS.waitpoint,
  "state", "closed",
  "closed_at", now_ms,
  "close_reason", "resumed"
)
redis.call("HSET", KEYS.suspension_current,
  "closed_at", now_ms,
  "close_reason", "resumed"
)
redis.call("ZREM", KEYS.suspension_timeout, ARGV.execution_id)

return ok()
```

### Atomic cancel-suspension function

`ff_cancel_suspension`

Cancels a suspended execution. Closes the waitpoint and suspension, terminates the attempt, closes the stream, and transitions the execution to terminal cancelled.

Keys:

- execution core
- current attempt record
- suspension current hash
- waitpoint hash
- waitpoint condition hash
- suspension timeout zset
- lane suspended index
- lane terminal index
- stream metadata (for attempt stream close)

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.core)
local suspension = redis.call("HGETALL", KEYS.suspension_current)

-- Validate execution is suspended
if core.lifecycle_phase ~= "suspended" then
  return err("execution_not_suspended")
end
if suspension.suspension_id == nil or suspension.suspension_id == "" then
  return err("execution_not_suspended")
end

-- OOM-SAFE WRITE ORDERING (per RFC-010 §4.8b):
-- exec_core HSET is the "point of no return" — write it FIRST.
-- If OOM kills after exec_core but before closing sub-objects,
-- execution is terminal:cancelled (correct) with stale suspension/waitpoint
-- records. The generalized index reconciler adds to terminal set.
-- Reverse order would leave sub-objects closed while exec_core still
-- says 'suspended' — reconciler would try to keep it suspended.

-- Step 1: Transition exec_core (FIRST — point of no return, all 7 dims)
redis.call("HSET", KEYS.core,
  "lifecycle_phase", "terminal",
  "ownership_state", "unowned",
  "eligibility_state", "not_applicable",
  "blocking_reason", "none",
  "blocking_detail", "",
  "terminal_outcome", "cancelled",
  "attempt_state", "attempt_terminal",
  "public_state", "cancelled",
  "cancellation_reason", ARGV.reason,
  "completed_at", now_ms,
  "last_transition_at", now_ms,
  "last_mutation_at", now_ms,
  "current_suspension_id", "",
  "current_waitpoint_id", "")

-- Step 2: Terminate attempt: suspended → ended_cancelled
redis.call("HSET", KEYS.attempt_record,
  "attempt_state", "ended_cancelled",
  "ended_at", now_ms,
  "failure_reason", ARGV.reason,
  "suspended_at", "",
  "suspension_id", "")

-- Step 3: Close attempt stream if it exists
if redis.call("EXISTS", KEYS.stream_meta) == 1 then
  redis.call("HSET", KEYS.stream_meta,
    "closed_at", now_ms,
    "closed_reason", "attempt_cancelled")
end

-- Step 4: Close sub-objects (safe to lose on OOM — stale but not zombie)
redis.call("HSET", KEYS.waitpoint,
  "state", "closed",
  "closed_at", now_ms,
  "close_reason", "cancelled")
redis.call("HSET", KEYS.wp_condition,
  "closed", "1",
  "closed_at", now_ms,
  "closed_reason", "cancelled")
redis.call("HSET", KEYS.suspension_current,
  "closed_at", now_ms,
  "close_reason", "cancelled")

-- Step 5: Remove from suspension indexes
redis.call("ZREM", KEYS.suspension_timeout, ARGV.execution_id)
redis.call("ZREM", KEYS.suspended_index, ARGV.execution_id)

-- Add to terminal set for retention scanning
redis.call("ZADD", KEYS.terminal_index, now_ms, ARGV.execution_id)

return ok()
```

### Pending waitpoint creation

`ff_create_pending_waitpoint`

Rules:

- requires the caller to still hold the active lease for the current attempt
- writes `state = pending`
- sets a short `expires_at`
- inserts the waitpoint into `ff:idx:{p:N}:pending_waitpoint_expiry`
- does not change execution lifecycle or lease ownership

### Timeout scanner (`ff-engine::scanner::suspension_timeout`)

Suspension timeouts are indexed in:

- `ff:idx:{p:N}:suspension_timeout`

Scanner pattern:

1. `ZRANGEBYSCORE ff:idx:{p:N}:suspension_timeout -inf now LIMIT 0 batch_size`
2. for each execution:
   - run `FCALL ff_expire_suspension`
   - revalidate that the execution is still suspended and the timeout is still due
   - apply the configured timeout behavior atomically

### Pending waitpoint expiry scanner (`ff-engine::scanner::pending_waitpoint_expiry`)

Pending waitpoint expiries are indexed in:

- `ff:idx:{p:N}:pending_waitpoint_expiry`

Scanner pattern:

1. `ZRANGEBYSCORE ff:idx:{p:N}:pending_waitpoint_expiry -inf now LIMIT 0 batch_size`
2. for each waitpoint:
   - if still `pending`, set state to `expired`
   - set `close_reason = never_committed`
   - remove it from the pending-expiry index
   - DEL `ff:wp:{p:N}:<waitpoint_id>:signals` (waitpoint signal stream — may have buffered signals that were never evaluated)
   - DEL `ff:wp:{p:N}:<waitpoint_id>:condition` (condition hash, if created)
   - for each signal_id in the signal stream: DEL `ff:signal:{p:N}:<signal_id>` and `ff:signal:{p:N}:<signal_id>:payload` (clean up orphaned signal records)

## Interactions with Other Primitives

- **RFC-001 Execution**: suspension is a first-class lifecycle state and drives `public_state = suspended`.
- **RFC-002 Attempt**: the active attempt must leave running state on suspend; resume continues with durable continuation context and no active lease while suspended.
- **RFC-003 Lease**: entering suspension atomically releases the lease and clears current ownership.
- **RFC-005 Signal**: signal acceptance and waitpoint matching are defined there; this RFC defines the waiting-side storage and satisfaction semantics.
- **Crate mapping**: `ff-sdk` calls `FCALL ff_suspend_execution` and `FCALL ff_create_pending_waitpoint`. `ff-server` API decodes `waitpoint_key` via `ff-core`, routes `FCALL ff_deliver_signal` to the correct `{p:N}` partition via `ff-script` (signal delivery is single-partition — no `ff-engine::dispatch` involvement). `ff-engine::scanner::suspension_timeout` and `ff-engine::scanner::pending_waitpoint_expiry` enforce time-based policies.

## V1 Scope

In v1:

- single active suspension per execution
- one active waitpoint per active suspension
- pending waitpoints for early callback safety
- multi-signal waitpoints with `any` and `all`
- explicit timeout behavior on suspension
- atomic suspend and atomic resume-to-runnable
- durable per-waitpoint signal buffer
- explicit signal vs control boundary

Designed for later but not required in v1:

- ordered signal matching
- richer operator-driven condition rewrites
- multi-waitpoint suspension graphs inside one suspension episode
- cross-execution waitpoint joins

## Open Questions

No unresolved questions remain.

**Resolved:** Q1 — Waitpoint key storage. Store raw `waitpoint_key` values. The raw token is needed for operator inspection ("what key was given to the external system?") and for re-issuance if the external system loses it. Storing only a keyed hash would prevent reconstruction. The MAC provides integrity; the raw value provides debuggability. Current design is correct.

**Resolved:** Q2 — Escalate timeout behavior. Deferred and aligned with RFC-005 Q1. When implemented, `escalate` will mutate the current suspension in place (change `reason_code` to `waiting_for_operator_review`, replace resume condition with operator-only). Mutate-in-place avoids the complexity of closing + creating a new suspension episode atomically. The waitpoint remains open; the condition is replaced.

**Resolved:** Q3 — Continuation metadata ownership. Pointer only — the engine stores `continuation_metadata_pointer` as an opaque string reference to worker-owned durable state. The engine does not own or parse the continuation data. This cleanly separates engine durability (suspension correctness) from runtime semantics (what the worker needs to resume). A standardized checkpoint envelope is a v2 extension if cross-runtime portability is needed.

## References

- Pre-RFC: `flowfabric_use_cases_and_primitives (2).md`
- Related RFCs: RFC-001, RFC-002, RFC-003, RFC-005
