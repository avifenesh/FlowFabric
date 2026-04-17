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

`waitpoint_key` is an externally opaque, internally routable string. The V1 implementation uses the simple `wpk:<uuid>` format and is **NOT** a crypto token. Multi-tenant signal authentication is enforced separately by the HMAC waitpoint token — see §Waitpoint Security below, which is the authoritative specification for the token mechanism in this RFC.

Routing to `{p:N}` is done by storing the partition number on the waitpoint hash at creation time, keyed by the waitpoint_id embedded in the `wpk:<uuid>` handle. No signature check on the handle itself; integrity is provided by the HMAC token the signal deliverer must present.

(An earlier draft of this RFC described a versioned 75-byte base64url token with embedded partition bytes and HMAC-SHA256 dual-key verification. That design is **not** what shipped — it was superseded by the separate HMAC-SHA1 waitpoint token scheme in §Waitpoint Security. See that section for the shipped contract: algorithm choice, kid-ring rotation, grace window semantics, and KEYS/ARGV impact.)

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

This function is called after a matching signal or explicit operator resume trigger has already been durably accepted.

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

### Proactive waitpoint closure

`ff_close_waitpoint`

Called by `ff-sdk` when the worker decides not to suspend after creating a pending waitpoint (SDK §10.3 step 7d). Prevents orphaned pending waitpoints.

Pseudocode:

```lua
-- KEYS: exec_core, waitpoint_hash, pending_wp_expiry_zset
-- ARGV: waitpoint_id, reason, now_ms

local wp = redis.call("HGETALL", KEYS.waitpoint_hash)
if not wp.waitpoint_id then
  return err("waitpoint_not_found")
end
if wp.state ~= "pending" and wp.state ~= "active" then
  return err("waitpoint_not_open")
end

redis.call("HSET", KEYS.waitpoint_hash,
  "state", "closed",
  "closed_at", ARGV.now_ms,
  "close_reason", ARGV.reason
)

-- Remove from pending expiry index (no-op if not pending)
redis.call("ZREM", KEYS.pending_wp_expiry_zset, ARGV.waitpoint_id)

return ok()
```

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

## Waitpoint Security (HMAC token authentication)

Multi-tenant deployments require that external signal sources (approval gates, webhook callbacks, tool result delivery) cannot deliver signals to waitpoints they do not own. Waitpoint IDs are UUIDs and can be unguessable, but they leak through logs, URLs, and inter-service hops. Relying on ID secrecy alone is insufficient.

Each waitpoint carries an **HMAC token** that the signal deliverer must present. The token is minted at waitpoint creation time, bound to the specific `(waitpoint_id, waitpoint_key, created_at)` tuple, and validated in Lua before any mutating signal-delivery work runs.

### Token format

`"kid:40hex"`, where:

- `kid` — short key identifier (e.g. `k1`, `k2`), enabling zero-downtime rotation
- `40hex` — HMAC-SHA1 digest, lowercase hex (SHA1 output = 20 bytes = 40 hex chars)

### Algorithm: HMAC-SHA1

The Valkey Lua sandbox exposes `redis.sha1hex` but no SHA-256 primitive. Rather than import a ~200 LOC pure-Lua SHA-256 (large attack surface, new crypto primitive shipped in scripts), we use **HMAC-SHA1 per RFC 2104**. The security property we need is MAC unforgeability, not hash collision resistance: HMAC-SHA1 remains unbroken as a MAC construction (NIST SP 800-107r1), and with a 32-byte random secret the forgery work factor is 2^160. The helper `hmac_sha1_hex` in `lua/helpers.lua` implements the standard ipad/opad construction with a 64-byte SHA1 block size.

### HMAC input (binding)

```
HMAC_SHA1( secret, waitpoint_id || "|" || waitpoint_key || "|" || created_at_ms )
```

- The pipe delimiter prevents field-boundary collisions across distinct `(waitpoint_id, waitpoint_key)` pairs.
- Binding `waitpoint_id` ensures a token for waitpoint A cannot authenticate against waitpoint B (even on the same execution).
- Binding `created_at_ms` means a waitpoint that is closed and later re-created at the same `(id, key)` tuple would mint a new token — the old token does not authenticate the new waitpoint.

### Constant-time comparison

Token validation compares the computed HMAC against the presented digest using `constant_time_eq` (helpers.lua): XOR-accumulate every byte, return equal iff the accumulator is zero. This closes remote timing attacks on early-exit character comparison.

### Secret storage and rotation

Secrets live in a per-partition hash: `ff:sec:{p:N}:waitpoint_hmac`. Fields:

| Field | Purpose |
| --- | --- |
| `current_kid` | The kid currently minting new tokens. |
| `secret:<kid>` | Hex-encoded HMAC key for kid `<kid>`. |
| `previous_kid` | The kid immediately preceding `current_kid`. |
| `previous_expires_at` | Absolute ms timestamp after which `previous_kid` tokens are rejected. |

**Per-partition replication is required** because Valkey cluster mode demands that all KEYS in a single FCALL hash to the same slot. A single global secret key (e.g. `{s:waitpoint}:hmac_secrets`) would produce CROSSSLOT errors in suspension.lua and signal.lua FCALLs, which already touch `{p:N}`-tagged keys. The secret value is **identical across partitions**; only the storage is replicated. Rotation fans out across partitions (`O(num_execution_partitions)` HSETs — 256 by default, bounded at deployment time via `FF_EXEC_PARTITIONS`).

### Key rotation (2-kid ring)

1. Operator POSTs new secret to `/v1/admin/rotate-waitpoint-secret` (FF_API_TOKEN-authenticated).
2. Server promotes `current_kid` → `previous_kid`, writes `previous_expires_at = now + FF_WAITPOINT_HMAC_GRACE_MS` (default 24h), installs the new kid + secret as current.
3. Both `current_kid` and `previous_kid` validate during the grace window. After `previous_expires_at`, `previous_kid` tokens return `token_expired`.
4. Fanout uses plain HSET (not HSETNX) and is **idempotent**: a partial-failure retry converges to the same state. The rotation endpoint returns `{rotated: N_ok, failed: [partition_ids]}`; HTTP 200 if any partition succeeded, 500 only if all fail. Per-partition outcomes are audit-logged (`target: "audit"`).

### Boot initialization (fail-fast)

`ff-server`'s `initialize_waitpoint_hmac_secret` runs between partition-config validation and Lua library load. For each execution partition:

1. HGET `current_kid`. If absent, HSET `current_kid=k1`, `secret:k1=<env hex>`.
2. If present and the stored secret matches env, continue (restart-safe).
3. If present and divergent, log a warning (operator rotation scenario) and preserve the stored secret.

Any HSET failure aborts the boot. A server running with some partitions missing the secret would `hmac_secret_not_initialized` half of traffic — failing loud at boot is strictly safer.

### Replay bounding via waitpoint state machine

Tokens are not serial-numbered and do not carry a nonce. Replay resistance comes from the waitpoint's one-shot state machine, not the token itself:

- `ff_deliver_signal` (see `lua/signal.lua` §3) checks `wp_cond.closed == "1"` and returns `waitpoint_closed` before any mutation.
- Once the resume condition is satisfied, `ff_deliver_signal` writes `closed=1` atomically as part of the same FCALL that records the signal.
- Therefore a leaked token cannot be replayed after the resume condition is met: even if the attacker controls the signal delivery endpoint, the next FCALL observes `closed=1` and rejects.

### Error codes (Lua → typed `ScriptError`)

| Code | Meaning |
| --- | --- |
| `missing_token` | ARGV token empty or non-string. |
| `invalid_token` | Malformed, unknown kid, wrong length, or HMAC mismatch. |
| `token_expired` | Previous kid accepted, but `previous_expires_at < now`. |
| `hmac_secret_not_initialized` | Boot init skipped or partition secret hash missing. |
| `invalid_keys_missing_hmac` | FCALL built without the hmac_secrets KEY (caller misuse). |

### KEYS / ARGV impact (library version 3)

| Function | KEYS | ARGV | Returns |
| --- | --- | --- | --- |
| `ff_create_pending_waitpoint` | +`hmac_secrets` | unchanged | +token |
| `ff_suspend_execution` | +`hmac_secrets` | unchanged | +token |
| `ff_deliver_signal` | +`hmac_secrets` | +`waitpoint_token` | unchanged |
| `ff_buffer_signal_for_pending_waitpoint` | +`hmac_secrets`, +`waitpoint_hash` | +`waitpoint_token` | unchanged |
| `ff_resume_execution`, `ff_expire_suspension`, `ff_close_waitpoint` | unchanged (internal, not externally triggered) | unchanged | unchanged |

This is a **breaking library-version bump** (2 → 3). Old SDK clients building pre-HMAC FCALL shapes will fail with arity or `invalid_keys_missing_hmac` errors, which is the desired behavior for multi-tenant security: no silent degradation.

### Required deployment configuration

Every `ff-server` deployment MUST set these environment variables. The server refuses to boot without them so no deployment can silently ship without HMAC protection.

| Env var | Required | Default | Notes |
| --- | --- | --- | --- |
| `FF_WAITPOINT_HMAC_SECRET` | **yes** | — | Hex-encoded HMAC-SHA1 signing secret. Recommended 64 hex chars (32 bytes). Even-length `0-9a-fA-F` validated at boot; malformed value fails `Server::start`. Store in a secret manager; never commit. |
| `FF_WAITPOINT_HMAC_GRACE_MS` | no | `86_400_000` (24h) | Window during which `previous_kid` tokens continue to validate after rotation. Tighten for high-sensitivity tenants, loosen for longer-running approval workflows. |
| `FF_API_TOKEN` | strongly recommended | — | Bearer token gating the entire API surface **including `/v1/admin/rotate-waitpoint-secret`**. Without it, admin endpoints are unauthenticated and the server logs a loud warning at boot. |

### Audit events

Rotation and other security-sensitive paths emit structured log events at `tracing::target: "audit"`. Operators aggregating audit logs should consume this target explicitly — the subscriber's default `EnvFilter` includes an `audit=info` directive out of the box; custom subscribers must add it.

| Event name (message) | Level | Emitted on | Structured fields |
| --- | --- | --- | --- |
| `waitpoint_hmac_rotation_complete` | INFO | Every rotate endpoint call (success or partial) | `new_kid`, `total_partitions`, `rotated`, `failed_count`, `in_progress_count` |
| `waitpoint_hmac_rotation_failed` | ERROR | Per-partition actual failure (Valkey outage, cluster split) | `partition`, `err` |
| `waitpoint_hmac_rotation_timeout_http_504` | ERROR | Rotate endpoint exceeds 120s server-side timeout | `new_kid`, `timeout_s` |

Per-partition `waitpoint_hmac_rotated` and `waitpoint_hmac_rotation_in_progress_skipped` are logged at `DEBUG` (not audit) because emitting 256 events per rotation blows up paid log aggregator budgets with no additional compliance value over the single aggregated INFO summary.

Event names and structured field keys are a **stable public contract**. Breaking changes require a library-version bump and a deprecation window.

### Reviewer token access

The HMAC `waitpoint_token` is returned to the suspending worker in `SuspendOutcome`. A human-in-the-loop reviewer or other non-worker principal that must deliver a signal against the waitpoint has no direct path to that token — it is not echoed in stream frames (frames are observability, not credential carriers) and not written to `exec_core`'s public surface.

`GET /v1/executions/{id}/pending-waitpoints` returns the actionable (`pending`/`active`) waitpoints for an execution, including `waitpoint_token`. It is subject to the same bearer-auth middleware (`FF_API_TOKEN`) as every other endpoint — no separate gate. Closed waitpoints are elided (history consumers should read the lease/stream record instead).

Callers pass the returned token unchanged into `DeliverSignalArgs::waitpoint_token`; tampering surfaces at the Lua HMAC validation path as `invalid_token` (see §Waitpoint Security).

### Performance notes

HMAC-SHA1 minting and validation cost scale with the input size, not the stream rate. The input is bounded: `waitpoint_id` (36 bytes UUID) + `|` + `waitpoint_key` (≤256 bytes per RFC-004's opaque-string ceiling) + `|` + `created_at_ms` (13 bytes) ≈ 310 bytes worst case. `redis.sha1hex` on a ~310-byte buffer runs in low hundreds of nanoseconds per call. In practice, throughput is dominated by the FCALL round-trip and XADD/HSET work, not the HMAC itself.

### Operator observability

Every `invalid_token`, `token_expired`, `missing_token`, or `invalid_keys_missing_hmac` rejection on the signal-delivery path (`ff_deliver_signal` or `ff_buffer_signal_for_pending_waitpoint`) writes `last_hmac_validation_failed_at = now_ms` on the target execution's `exec_core` hash. Bounded scalar — one write per rejection, no per-failure log volume.

Operators correlate this field with key rotation windows, client deploys, or attack traffic without tailing Lua slowlog:

```
HGET ff:exec:{p:42}:<eid>:core last_hmac_validation_failed_at
```

Absent field → never failed. Present field → last timestamp of any auth failure. Pair with execution lifecycle telemetry to distinguish "aged token, benign" from "stuck waitpoint, needs rotation audit".

Audit-log correlation: every rotation emits the `waitpoint_hmac_rotation_complete` event (see §Audit events); `last_hmac_validation_failed_at` timestamps falling shortly after a rotation suggest in-flight tokens that pre-dated the rotation. Outside rotation windows, concentrated timestamps on one execution suggest a client bug or a probe.

### Rate-limit posture

HMAC validation is cheap (see above) but the entire `ff_deliver_signal` FCALL runs inside Valkey's single-threaded Lua slot for one `{p:N}` partition. A flood of tampered-token signals against one partition (same `execution_id` or a tight cluster of IDs mapped to the same `{p:N}`) saturates that shard's Lua slot even though every call returns `invalid_token` fast.

v1 posture: **defer to LB/WAF for coarse rate limits.** The HMAC check is cheap enough that a determined attacker's throughput is bounded by their own connection concurrency against the LB, which is where rate limits belong. Per-(execution_id, source) token-bucket admission at the REST boundary is a deferred v2 feature if operators see partition starvation from hostile signal traffic.

No in-server backpressure is wired today. Operators should monitor Valkey slowlog on the signal-delivery script family for evidence of hot partitions and front the API with an LB-level rate limiter (e.g. Cloudflare, ALB, nginx `limit_req`) sized for expected legitimate signal volume plus headroom.

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
- HMAC-SHA1 waitpoint tokens with 2-kid rotation (see §Waitpoint Security)

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

---

## Implementation Notes (v1)

- **`waitpoint_key` uses simple `wpk:<uuid>` format** and is not itself a crypto token. Multi-tenant signal authentication is provided by the separate **waitpoint HMAC token** (§Waitpoint Security), returned alongside the waitpoint_id at creation and required on all signal-delivery FCALLs. The HMAC uses SHA1 (in-engine via `redis.sha1hex`) rather than SHA-256 (which is not exposed in the Valkey Lua sandbox). The routing problem is solved by storing the partition number on the waitpoint hash and looking it up via the waitpoint_id embedded in the key.
- **`auto_resume_with_timeout_signal` does not append a synthetic timeout signal.** The timeout scanner triggers resume directly without injecting a signal into the waitpoint buffer. Synthetic signal injection is deferred to v2.
- **Suspension timeout scanner and pending waitpoint expiry scanner are fully implemented** as Rust-only scanners in `ff-engine::scanner`. They use `ZRANGEBYSCORE` on the respective timeout indexes.
- **OOM-safe write ordering is enforced.** `ff_suspend_execution` writes `exec_core` HSET first (lifecycle → suspended), then creates sub-objects. `ff_resume_execution` writes `exec_core` first (lifecycle → runnable), then closes sub-objects.
