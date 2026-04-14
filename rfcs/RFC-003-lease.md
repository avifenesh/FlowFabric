# RFC-003: Lease and Fencing Semantics

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md

---

## Summary

This RFC defines the lease object, ownership invariants, fencing rules, lifecycle, error model, and the Valkey-native atomicity model for FlowFabric execution ownership. A lease is the engine's proof that exactly one worker attempt is allowed to mutate active execution state at a time. The design in this RFC is the correctness boundary that prevents split-brain execution, stale completion after reclaim, and silent corruption during crash recovery.

## Motivation

This RFC is driven primarily by UC-11 through UC-18 and UC-26 from the pre-RFC:

- UC-11 lease-based ownership
- UC-12 crash recovery / stalled reclaim
- UC-13 cancellation / revocation
- UC-14 timeout enforcement
- UC-15 pause / drain / freeze intake
- UC-16 manual requeue / replay
- UC-17 state inspection
- UC-18 operator override
- UC-26 preemption / handoff

Without an explicit lease and fencing model, FlowFabric cannot safely support long-running execution, reclaim after worker loss, operator intervention, or any replay/reclaim semantics defined in RFC-002.

## Detailed Design

### Object Definition

A lease is a durable, inspectable ownership object attached to one execution and exactly one attempt of that execution.

The lease is modeled as a current lease record plus immutable history entries. The current lease record is authoritative for mutation validation. History exists for audit and operator diagnosis.

#### Lease object

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `lease_id` | opaque string | yes | Unique per lease issuance. New on every acquire, reclaim, and handoff. |
| `lease_epoch` | u64 | yes | Monotonic fencing token per execution. Increments on each new lease issuance. Does not change on renew. |
| `execution_id` | string | yes | Stable logical execution identity. |
| `attempt_index` | u32 | yes | Zero-based attempt index for the attempt currently bound to the lease. Primary lease-attempt binding key in script validation. |
| `attempt_id` | string | yes | The lease belongs to the currently running attempt defined by RFC-002. |
| `worker_id` | string | yes | Logical worker identity. |
| `worker_instance_id` | string | yes | Concrete worker process / runtime instance identity. |
| `capability_snapshot` | object or canonical hash | yes | Snapshot of the routing capability set that justified placement. Stored for audit and deterministic reclaim explanation. |
| `lane` | string | yes | Lane / queue / route bucket used at acquisition time. |
| `acquired_at` | unix ms | yes | Lease issuance time, using Valkey server time. |
| `expires_at` | unix ms | yes | Hard ownership deadline. At or after this instant, the lease is invalid. |
| `last_renewed_at` | unix ms | yes | Last successful renew time. Equal to `acquired_at` at creation. |
| `renewal_deadline` | unix ms | yes | Recommended latest time to renew before expiration. Used for worker behavior and early warning, not as the hard fence. |
| `reclaim_count` | u32 | yes | Count of reclaim-driven ownership turnovers for this execution. |
| `previous_lease_id` | nullable string | no | Previous lease in the chain, used for reclaim and handoff lineage. |
| `revoked_at` | nullable unix ms | no | Set when the lease was intentionally revoked. |
| `revoke_reason` | nullable string | no | Operator or system reason for revocation. |

#### Execution fields that mirror current lease state

The execution core record must mirror enough lease state to validate mutations without relying on a separate lookup:

- `current_lease_id`
- `current_lease_epoch`
- `current_attempt_id`
- `current_attempt_index`
- `current_worker_id`
- `current_worker_instance_id`
- `current_lane`
- `lease_acquired_at`
- `lease_expires_at`
- `lease_last_renewed_at`
- `lease_renewal_deadline`
- `lease_reclaim_count`
- `lease_expired_at`
- `ownership_state`

The current lease hash is detailed auditable state. The execution record is the authoritative mutation fence.

### Invariants

#### L1. At most one valid lease

At any time, an execution may have at most one valid current lease.

Rationale:
- prevents duplicate active owners
- gives operators a single source of truth
- makes acquisition/reclaim a compare-and-swap problem instead of conflict resolution after the fact

#### L2. Lease required for active mutation

Any normal active-state mutation must include and validate the current `lease_id` and `lease_epoch`.

This includes:
- complete
- fail
- suspend
- transition to waiting-children
- transition to delayed
- progress / usage mutation when owner validation is enforced

Rationale:
- the execution must not accept mutations from a worker that no longer owns it
- mutation safety cannot be inferred from timing alone

#### L3. Stale owner rejection

A worker that previously held a lease but lost it through expiry, revoke, reclaim, or handoff must be rejected even if it is still running.

Rationale:
- TTL alone is not sufficient because an old worker can wake up late and try to complete
- monotonic fencing is required to reject stale completion deterministically

#### L4. Expiration is observable

Lease expiry must be visible in the execution state model and audit trail. An expired lease is not just a missing heartbeat; it is an inspectable ownership condition.

Rationale:
- operators need to know why an execution is reclaimable
- reclaim logic must not depend on ambiguous absence alone

#### L5. Suspension releases ownership

A suspended execution must not retain a valid current lease.

Rationale:
- suspended work is intentionally not running
- retaining active ownership while waiting would block reclaim, routing changes, and operator reasoning

#### L6. Completion clears ownership

Completed, Failed, Cancelled, and Expired executions must not retain a valid current lease.

Rationale:
- terminal executions must be obviously unowned
- stale post-terminal renewal or progress attempts must fail cleanly

### Fencing Model

FlowFabric uses a monotonic fencing token, `lease_epoch`, per execution.

Rules:

1. The epoch starts at `0` when the execution is created with no lease.
2. Every successful new lease issuance increments the epoch:
   - initial acquire
   - reclaim after expiry
   - force reclaim after revoke
   - future cooperative handoff
3. Renew does not increment the epoch.
4. Release does not increment the epoch.
5. Every active-state mutation must include:
   - `execution_id`
   - `attempt_index`
   - `attempt_id`
   - `lease_id`
   - `lease_epoch`
6. The engine validates the mutation against the execution's current lease summary, not against worker memory.

Validation rule:

- accept only if `current_attempt_index == expected_attempt_index`, `current_lease_id == expected_lease_id`, and `current_lease_epoch == expected_lease_epoch`
- otherwise reject as `stale_lease`, `lease_expired`, or `lease_revoked` according to the current ownership state

This is the split-brain fence. Once a later lease wins, older owners can never mutate active state again.

### Lease Lifecycle

#### Acquire

A worker claims a runnable execution and receives a new lease for the current pending attempt.

Effects:
- increments `lease_epoch`
- creates a new `lease_id`
- sets `ownership_state = leased`
- sets `lifecycle_phase = active`
- clears waiting-style blocking reasons

#### Renew

The current owner extends `expires_at` before it lapses.

Effects:
- preserves `lease_id`
- preserves `lease_epoch`
- updates `last_renewed_at`, `renewal_deadline`, `expires_at`

#### Release

The current owner voluntarily yields ownership as part of a legal state transition.

Legal release targets:
- `active -> suspended`
- `active -> waiting_children`
- `active -> delayed`
- terminal transitions that clear ownership

A naked "drop lease but remain active and unowned" path is forbidden.

#### Expire

The owner fails to renew by `expires_at`.

Effects:
- current owner becomes invalid immediately
- execution becomes reclaimable
- stale mutations are rejected even if the scanner has not run yet
- ownership state becomes `lease_expired_reclaimable`

#### Reclaim

Another worker claims an execution whose lease expired or was explicitly revoked for recovery.

Effects:
- creates a new attempt on the same execution
- increments `lease_epoch`
- creates a new `lease_id`
- sets `previous_lease_id`
- increments `reclaim_count`

#### Revoke

The system or an operator intentionally invalidates the current lease.

Typical reasons:
- worker drain
- safety stop
- operator intervention
- forced reroute

Revocation does not by itself complete or cancel the execution. It clears the current valid owner and places the execution into an inspectable non-trusted ownership condition.

#### Handoff

Handoff is a cooperative ownership turnover from one worker to another.

Decision for v1:
- designed for
- not required

If added later, handoff must be atomic and must issue a new `lease_id` and increment `lease_epoch`. A release-then-acquire sequence is not acceptable for handoff correctness.

### Acquisition Rules

A lease may be acquired only when all of the following are true:

1. `lifecycle_phase = runnable`
2. `ownership_state = unowned`
3. `eligibility_state = eligible_now`
4. `terminal_outcome = none`
5. the execution is attached to a claimable attempt
6. routing constraints are satisfied
7. the claimant's capability snapshot satisfies the execution's route requirements
8. fairness, quota, and concurrency admission checks passed
9. the execution is not paused, drained, or operator-blocked

Implications by public state:

- `waiting`: leaseable
- `delayed`: not directly leaseable; must first be promoted to runnable
- `rate_limited`: not directly leaseable; must first be admitted
- `waiting_children`: not directly leaseable; dependencies must resolve first
- `suspended`: not directly leaseable; resume must transition it back to runnable first
- `active`: not normally leaseable unless reclaim/force-reclaim rules apply
- terminal states: never leaseable

#### Admission model boundary

Capability, fairness, quota, and route scoring may be computed outside the lease script, but their final commit must be represented by durable claim context that the claim script validates atomically.

Recommended v1 shape:
- the scheduler/control plane computes routing and admission
- it issues a partition-local claim grant for one execution and one claimant
- `acquire_lease` consumes that grant atomically with lease creation

This avoids cross-slot reads against global worker registries while still making the final claim decision atomic.

### Renewal Rules

Renewal is allowed only when:

1. `lifecycle_phase = active`
2. `ownership_state = leased`
3. `current_lease_id` matches
4. `current_lease_epoch` matches
5. `current_attempt_index` matches
6. `current_attempt_id` matches
7. `now < lease_expires_at`
8. the lease has not been revoked
9. the execution is not terminal

Renewal must fail deterministically if:

- the lease was already reclaimed
- the execution transitioned away from active
- the lease was revoked
- `now >= lease_expires_at`

All expiry decisions use Valkey server time, not worker clocks.

### Expiration and Reclaim Rules

At `now >= expires_at`, the lease is invalid, even if:

- the current lease hash has not been garbage-collected
- the reclaim scanner has not yet processed the execution
- the old worker is still alive and sending mutations

Expiration handling rules:

1. owner mutations encountering an expired lease must fail
2. any mutation or renew path that notices expiry may atomically flip ownership state to `lease_expired_reclaimable`
3. a background scanner must mark and index expired executions so the condition is observable without waiting for a failed mutation
4. reclaim must be atomic:
   - verify the old lease is expired or revoked
   - create the new attempt
   - issue the new lease
   - bump the epoch
   - make the new owner authoritative in one script

Shared script helper:

- `mark_expired(core, now_ms)` updates the execution core atomically to:
  - `ownership_state = lease_expired_reclaimable`
  - `blocking_reason = waiting_for_worker`
  - `lease_expired_at = now_ms`
  - preserve the current lease identity fields for audit and stale-owner rejection
- the helper should be idempotent: if the execution is already marked `lease_expired_reclaimable`, it should not append duplicate history entries

The helper does not mint a new lease and does not create a new attempt. It only turns an overdue lease into an explicit reclaimable ownership condition.

### Ownership Conflict Rules

If multiple workers contend for the same execution:

- exactly one may win the new lease
- losers receive deterministic rejection
- stale owners may not complete after a winner exists

Recommended rejection precedence:

For acquire / reclaim:
1. `execution_not_leaseable` if lifecycle or eligibility forbids claim
2. `lease_conflict` if another valid lease already exists
3. `stale_lease` if a reclaim request references an older ownership generation

For renew / active mutation:
1. `execution_not_active` if lifecycle is no longer active
2. `lease_revoked` if revoke markers are present
3. `lease_expired` if `now >= lease_expires_at`
4. `stale_lease` if `attempt_index`, `attempt_id`, `lease_id`, or `lease_epoch` mismatch

### Interaction with Execution Transitions

#### Active -> Completed

Requires matching valid lease unless a privileged operator override is used.

Effects:
- marks current attempt terminal success
- clears current lease
- removes reclaimability
- clears ownership

#### Active -> Failed

Requires matching valid lease unless privileged override.

Effects:
- marks current attempt failed
- clears current lease
- if retry is not scheduled, terminal failure is recorded
- if retry is scheduled, ownership still clears before the next attempt is made runnable

#### Active -> Suspended

Requires matching valid lease.

Effects:
- records waitpoint / suspension data
- clears current lease
- sets `lifecycle_phase = suspended`
- sets `ownership_state = unowned`

#### Active -> WaitingChildren

Requires matching valid lease.

Effects:
- records dependency wait condition
- clears current lease
- sets `lifecycle_phase = runnable`
- sets `eligibility_state = blocked_by_dependencies`
- leaves the execution unowned

#### Active -> Delayed

Requires matching valid lease.

Effects:
- records next eligible time
- clears current lease
- sets `lifecycle_phase = runnable`
- sets `eligibility_state = not_eligible_until_time`

#### Active -> Cancelled

May be initiated by the owner or by privileged override.

Effects:
- clears current lease
- sets terminal cancelled outcome

### Operator Interactions

Operators must be able to:

- inspect the current lease
- inspect remaining lease time and ownership state
- inspect lease history
- revoke the current lease with a reason
- drain a worker instance by revoking all of its leases
- force reclaim an execution after safety checks

#### Drain semantics

Worker drain is a control-plane operation, not a worker mutation. Recommended flow:

1. mark the worker instance draining
2. enumerate current leases by worker-instance secondary index
3. for each candidate execution, re-read the execution core and revoke only if the core still shows that worker instance as the authoritative current owner
4. either:
   - allow the scheduler to reclaim naturally, or
   - force reclaim selected executions immediately

Correctness does not depend on the worker index being globally perfect. It is an operator aid. The execution-local lease validation remains the real fence.

### Operations

| Operation | Class | Semantics |
| --- | --- | --- |
| `acquire_lease(execution_id, claim_context)` | A | Atomically validates claimability and creates the first valid lease for the current attempt. |
| `renew_lease(execution_id, attempt_index, attempt_id, lease_id, lease_epoch)` | A | Extends a still-valid current lease. |
| `release_lease(execution_id, attempt_index, attempt_id, lease_id, lease_epoch, target_state)` | A | Clears ownership while performing a legal non-terminal transition. |
| `revoke_lease(execution_id, lease_id, reason)` | A | Invalidates the current lease and records the revoke reason. |
| `reclaim_execution(execution_id, claim_context)` | A | Atomically converts expired/revoked ownership into a new attempt plus new lease. |
| `get_lease(execution_id)` | C | Returns the current lease summary and ownership condition. |
| `get_lease_history(execution_id)` | C | Returns immutable lease history entries. |

## Valkey Data Model

### Key naming and partitioning

This RFC assumes Valkey Cluster compatibility. Atomic Lua in cluster mode requires all keys in a script to hash to the same slot. Therefore, FlowFabric must not use pure `{execution_id}` hash tags for lease operations if it also needs local indexes such as expiry sets.

Instead, each execution is assigned a stable partition tag:

- `partition_tag = p:<partition_number>`

Partition assignment algorithm:

- `partition_number = hash(execution_id) % num_partitions`
- the partition is computed exactly once when the execution is created
- `num_partitions` is fixed at deployment time for one cluster topology generation
- the chosen `partition_number` is stored on the execution core record and reused by every later script

This ensures that execution, lease, attempt, and local indexes remain scriptable in one slot without recomputing placement from mutable runtime data.

All keys that must participate in one atomic lease script share the same hash tag:

- `ff:exec:{p:N}:<execution_id>:core`
- `ff:exec:{p:N}:<execution_id>:lease:current`
- `ff:exec:{p:N}:<execution_id>:lease:history`
- `ff:exec:{p:N}:<execution_id>:attempts`
- `ff:attempt:{p:N}:<execution_id>:<attempt_index>`
- `ff:idx:{p:N}:lease_expiry`
- `ff:idx:{p:N}:lane:<lane>:eligible`
- `ff:idx:{p:N}:lane:<lane>:active`
- `ff:idx:{p:N}:worker:<worker_instance_id>:leases`
- `ff:exec:{p:N}:<execution_id>:claim_grant`

This lets one script update:
- the execution core record
- the current lease record
- the attempt pointer
- the attempt index and attempt detail keys
- the partition-local expiry index
- the partition-local eligible index
- the partition-local active index
- the worker-instance secondary index

in one atomic operation.

### Authoritative records versus secondary indexes

Authoritative for correctness:

- `...:core`
- `...:lease:current`

Secondary but maintained atomically where possible:

- `ff:idx:{p}:lease_expiry`
- `ff:idx:{p}:worker:<worker_instance_id>:leases`
- `ff:idx:{p}:lane:<lane>:eligible`
- `ff:idx:{p}:lane:<lane>:active`

Correctness decisions must always come from the authoritative execution state, not from the indexes alone.

### Execution core record

Recommended execution lease-related fields inside `ff:exec:{p}:<execution_id>:core`:

- `lifecycle_phase`
- `ownership_state`
- `eligibility_state`
- `blocking_reason`
- `terminal_outcome`
- `current_attempt_id`
- `current_attempt_index`
- `current_lease_id`
- `current_lease_epoch`
- `current_worker_id`
- `current_worker_instance_id`
- `current_lane`
- `lease_acquired_at`
- `lease_last_renewed_at`
- `lease_renewal_deadline`
- `lease_expires_at`
- `lease_reclaim_count`
- `lease_expired_at`
- `lease_revoked_at`
- `lease_revoke_reason`

### Current lease record

`ff:exec:{p}:<execution_id>:lease:current` stores the full lease object.

Recommended storage:
- hash for structured fields
- optional JSON blob for `capability_snapshot`

TTL strategy:
- set `PEXPIREAT` to `expires_at + lease_history_grace_ms`
- the TTL is for cleanup only
- lease validity is decided by the execution core fields and server time

### Lease history

`ff:exec:{p}:<execution_id>:lease:history` should be an append-only stream.

Recommended trim policy:

- use `XADD ... MAXLEN ~ <lease_history_maxlen>`
- trimming is approximate to keep the hot path bounded
- default retention should be large enough to preserve recent ownership lineage while long-term audit can be copied into colder storage

Recommended events:
- `acquired`
- `renewed`
- `released`
- `expired`
- `revoked`
- `reclaimed`
- `handoff_started`
- `handoff_completed`

Each entry should include:
- `lease_id`
- `lease_epoch`
- `attempt_id`
- `attempt_index`
- `worker_id`
- `worker_instance_id`
- `ts`
- `reason` if applicable

### Atomic claim script

`acquire_lease.lua`

Keys:

- execution core
- current lease hash
- lease history stream
- current attempt record
- partition-local lease expiry zset
- partition-local worker lease set
- partition-local eligible index
- partition-local active index
- claim grant

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)

local core = redis.call("HGETALL", KEYS.core)
assert_execution_exists(core)

if core.lifecycle_phase ~= "runnable" then
  return err("execution_not_leaseable")
end
if core.ownership_state ~= "unowned" then
  if core.ownership_state == "leased" and tonumber(core.lease_expires_at) > now_ms then
    return err("lease_conflict")
  end
  return err("execution_not_leaseable")
end
if core.eligibility_state ~= "eligible_now" then
  return err("execution_not_leaseable")
end
if core.terminal_outcome ~= "none" then
  return err("execution_not_leaseable")
end

local grant = redis.call("HGETALL", KEYS.claim_grant)
validate_claim_grant(grant, ARGV.worker_id, ARGV.worker_instance_id, ARGV.lane, ARGV.capability_snapshot_hash)

local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
local lease_id = ARGV.lease_id
local expires_at = now_ms + tonumber(ARGV.lease_ttl_ms)
local renewal_deadline = now_ms + tonumber(ARGV.renew_before_ms)

redis.call("HSET", KEYS.core,
  "lifecycle_phase", "active",
  "ownership_state", "leased",
  "blocking_reason", "none",
  "current_attempt_id", core.current_attempt_id,
  "current_attempt_index", core.current_attempt_index,
  "current_lease_id", lease_id,
  "current_lease_epoch", next_epoch,
  "current_worker_id", ARGV.worker_id,
  "current_worker_instance_id", ARGV.worker_instance_id,
  "current_lane", ARGV.lane,
  "lease_acquired_at", now_ms,
  "lease_last_renewed_at", now_ms,
  "lease_renewal_deadline", renewal_deadline,
  "lease_expires_at", expires_at,
  "lease_reclaim_count", core.lease_reclaim_count or "0",
  "lease_expired_at", "",
  "lease_revoked_at", "",
  "lease_revoke_reason", ""
)

redis.call("DEL", KEYS.current_lease)
redis.call("HSET", KEYS.current_lease,
  "lease_id", lease_id,
  "lease_epoch", next_epoch,
  "execution_id", ARGV.execution_id,
  "attempt_index", core.current_attempt_index,
  "attempt_id", core.current_attempt_id,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "capability_snapshot", ARGV.capability_snapshot_json,
  "lane", ARGV.lane,
  "acquired_at", now_ms,
  "expires_at", expires_at,
  "last_renewed_at", now_ms,
  "renewal_deadline", renewal_deadline,
  "reclaim_count", core.lease_reclaim_count or "0",
  "previous_lease_id", "",
  "revoked_at", "",
  "revoke_reason", ""
)
redis.call("PEXPIREAT", KEYS.current_lease, expires_at + tonumber(ARGV.lease_history_grace_ms))

redis.call("ZADD", KEYS.lease_expiry, expires_at, ARGV.execution_id)
redis.call("SADD", KEYS.worker_leases, ARGV.execution_id)
redis.call("ZREM", KEYS.eligible_index, ARGV.execution_id)
redis.call("ZADD", KEYS.active_index, expires_at, ARGV.execution_id)
redis.call("DEL", KEYS.claim_grant)
redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
  "event", "acquired",
  "lease_id", lease_id,
  "lease_epoch", next_epoch,
  "attempt_index", core.current_attempt_index,
  "attempt_id", core.current_attempt_id,
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "ts", now_ms
)

return ok(lease_id, next_epoch, expires_at)
```

### Atomic renew script

`renew_lease.lua`

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.core)

local function mark_expired(core, now_ms)
  if core.ownership_state == "lease_expired_reclaimable" and core.lease_expired_at ~= "" then
    return
  end
  redis.call("HSET", KEYS.core,
    "ownership_state", "lease_expired_reclaimable",
    "blocking_reason", "waiting_for_worker",
    "lease_expired_at", now_ms
  )
  redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
    "event", "expired",
    "lease_id", core.current_lease_id,
    "lease_epoch", core.current_lease_epoch,
    "attempt_index", core.current_attempt_index,
    "attempt_id", core.current_attempt_id,
    "ts", now_ms
  )
end

if core.lifecycle_phase ~= "active" then
  return err("execution_not_active")
end
if core.ownership_state == "lease_revoked" or core.lease_revoked_at ~= "" then
  return err("lease_revoked")
end
if tonumber(core.lease_expires_at) <= now_ms then
  mark_expired(core, now_ms)
  return err("lease_expired")
end
if tostring(core.current_attempt_index) ~= ARGV.attempt_index then
  return err("stale_lease")
end
if core.current_attempt_id ~= ARGV.attempt_id then
  return err("stale_lease")
end
if core.current_lease_id ~= ARGV.lease_id or tostring(core.current_lease_epoch) ~= ARGV.lease_epoch then
  return err("stale_lease")
end

local new_expires_at = now_ms + tonumber(ARGV.lease_ttl_ms)
local new_renewal_deadline = now_ms + tonumber(ARGV.renew_before_ms)

redis.call("HSET", KEYS.core,
  "lease_last_renewed_at", now_ms,
  "lease_renewal_deadline", new_renewal_deadline,
  "lease_expires_at", new_expires_at
)
redis.call("HSET", KEYS.current_lease,
  "last_renewed_at", now_ms,
  "renewal_deadline", new_renewal_deadline,
  "expires_at", new_expires_at
)
redis.call("PEXPIREAT", KEYS.current_lease, new_expires_at + tonumber(ARGV.lease_history_grace_ms))
redis.call("ZADD", KEYS.lease_expiry, new_expires_at, ARGV.execution_id)
redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
  "event", "renewed",
  "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch,
  "attempt_index", ARGV.attempt_index,
  "attempt_id", ARGV.attempt_id,
  "ts", now_ms
)

return ok(new_expires_at)
```

### Atomic complete / fail script

`complete_or_fail.lua`

This script is the template for any active-state mutation that clears ownership.

Pseudocode:

```lua
local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)
local core = redis.call("HGETALL", KEYS.core)

local function mark_expired(core, now_ms)
  if core.ownership_state == "lease_expired_reclaimable" and core.lease_expired_at ~= "" then
    return
  end
  redis.call("HSET", KEYS.core,
    "ownership_state", "lease_expired_reclaimable",
    "blocking_reason", "waiting_for_worker",
    "lease_expired_at", now_ms
  )
  redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
    "event", "expired",
    "lease_id", core.current_lease_id,
    "lease_epoch", core.current_lease_epoch,
    "attempt_index", core.current_attempt_index,
    "attempt_id", core.current_attempt_id,
    "ts", now_ms
  )
end

if core.lifecycle_phase ~= "active" then
  return err("execution_not_active")
end
if core.ownership_state == "lease_revoked" or core.lease_revoked_at ~= "" then
  return err("lease_revoked")
end
if tonumber(core.lease_expires_at) <= now_ms then
  mark_expired(core, now_ms)
  return err("lease_expired")
end
if tostring(core.current_attempt_index) ~= ARGV.attempt_index then
  return err("stale_lease")
end
if core.current_attempt_id ~= ARGV.attempt_id then
  return err("stale_lease")
end
if core.current_lease_id ~= ARGV.lease_id or tostring(core.current_lease_epoch) ~= ARGV.lease_epoch then
  return err("stale_lease")
end

apply_attempt_outcome(KEYS.current_attempt, ARGV.outcome, now_ms, ARGV.result_ref)
apply_execution_transition(KEYS.core, ARGV.outcome, now_ms, ARGV.next_state_fields)

redis.call("ZREM", KEYS.lease_expiry, ARGV.execution_id)
redis.call("ZREM", KEYS.active_index, ARGV.execution_id)
redis.call("SREM", KEYS.worker_leases, ARGV.execution_id)
redis.call("XADD", KEYS.lease_history, "MAXLEN", "~", ARGV.lease_history_maxlen, "*",
  "event", ARGV.outcome == "success" and "released" or "released",
  "lease_id", ARGV.lease_id,
  "lease_epoch", ARGV.lease_epoch,
  "attempt_index", ARGV.attempt_index,
  "attempt_id", ARGV.attempt_id,
  "reason", ARGV.outcome,
  "ts", now_ms
)
redis.call("DEL", KEYS.current_lease)
clear_current_lease_fields(KEYS.core)

return ok()
```

The same validation block is reused for:
- complete
- fail
- suspend
- delay
- move to waiting-children
- cooperative release into a legal non-active state

**Post-script caller obligation:** After this script returns, the caller must issue an async DECR to the quota concurrency counter on the `{q:K}` partition (RFC-008 §4.5). This is a cross-partition operation and cannot be included in this atomic script.

### Revoke and reclaim scripts

`revoke_lease.lua`:

- validate the target lease if an expected `lease_id` is supplied
- set `ownership_state = lease_revoked`
- set `lease_revoked_at` and `lease_revoke_reason`
- remove the execution from the worker-lease set
- remove from the expiry zset
- append a `revoked` history event using `XADD ... MAXLEN ~ <lease_history_maxlen>` with fields: `event`, `lease_id`, `lease_epoch`, `attempt_index`, `attempt_id`, `worker_id`, `worker_instance_id`, `reason`, `ts`

**Post-script caller obligation:** After this script returns, the caller must issue an async DECR to the quota concurrency counter on the `{q:K}` partition (RFC-008 §4.5).

`reclaim_execution.lua`:

- validate `ownership_state in {lease_expired_reclaimable, lease_revoked}`
- verify the caller's claim grant
- load `current_attempt_index` from the execution core and derive `next_attempt_index = current_attempt_index + 1`
- write the previous attempt terminal detail needed for reclaim history
- create the next attempt keys defined by RFC-002:
  - `ff:attempt:{p}:<execution_id>:<next_attempt_index>`
  - `ff:attempt:{p}:<execution_id>:<next_attempt_index>:usage`
  - `ff:attempt:{p}:<execution_id>:<next_attempt_index>:policy`
  - `ff:exec:{p}:<execution_id>:attempts`
- update `current_attempt_index` on the execution core
- increment `lease_epoch`
- create the new current lease
- append `expired` and `reclaimed` history entries if needed, each with fields: `event`, `lease_id`, `lease_epoch`, `attempt_index`, `attempt_id`, `worker_id`, `worker_instance_id`, `reason`, `ts`
- restore `ownership_state = leased`
- set `lifecycle_phase = active`
- remove any stale active/eligible membership with `ZREM`
- insert the new authoritative membership with `ZADD` into the correct active or eligible index for the post-reclaim state
- refresh `ff:idx:{p}:lease_expiry` with the new `expires_at`
- update the worker-instance lease set for the new owner

**Post-script caller obligation:** After this script returns, the caller must issue an async DECR to the quota concurrency counter on the `{q:K}` partition for the old worker, and an async INCR for the new worker (RFC-008 §4.5).

Reclaim is never modeled as "delete old lease, then later maybe create a new one". That gap would reintroduce split-brain risk and break audit clarity.

### TTL-based expiry detection

TTL is helpful but not sufficient.

Rules:

1. `lease_expires_at` in the execution core is authoritative.
2. `PEXPIREAT` on `...:lease:current` is cleanup and operator convenience only.
3. Expiry decisions must compare `now_ms` against `lease_expires_at`.
4. The engine must not rely on keyspace notifications for correctness.

### Reclaim scan pattern

Each partition has a local expiry index:

- `ff:idx:{p}:lease_expiry`

Members:
- member = `execution_id`
- score = `lease_expires_at`

Background scanner pattern:

1. for each partition `p`
2. `ZRANGEBYSCORE ff:idx:{p}:lease_expiry -inf now LIMIT 0 batch_size`
3. for each returned execution:
   - run `mark_lease_expired_if_due.lua` or `reclaim_execution.lua`
   - the script re-validates the execution core fields
   - if the lease was renewed in the meantime, the script is a no-op

This makes expiry observable without trusting passive key deletion.

## Error Model

| Error | Meaning |
| --- | --- |
| `lease_not_found` | No current lease exists for the execution, or the requested historical lease record does not exist. |
| `lease_expired` | The supplied lease existed but `now >= lease_expires_at`, so it is no longer valid. |
| `stale_lease` | `attempt_index`, `attempt_id`, `lease_id`, or `lease_epoch` does not match the current authoritative owner. |
| `lease_conflict` | A different valid current lease already exists, so the claim loses. |
| `lease_revoked` | The lease was intentionally revoked and must not be renewed or used for mutation. |
| `execution_not_leaseable` | The execution's lifecycle/eligibility/terminal state does not permit acquisition or reclaim. |
| `execution_not_active` | An active-owner mutation was attempted against an execution that is no longer in active phase. |

## Interactions with Other Primitives

- **RFC-001 Execution**: execution state vector, public-state derivation, and legal transitions are the base that lease mutations act on.
- **RFC-002 Attempt**: reclaim creates a new attempt on the same execution; lease ownership is attempt-scoped.
- **RFC-004 Suspension**: suspension explicitly clears ownership and never retains a valid lease.
- **Scheduling / Admission RFC (later)**: route scoring, fairness, quota, and claim-grant issuance feed `acquire_lease` but do not weaken its atomicity requirements.

## V1 Scope

In v1:

- single active lease per execution
- monotonic `lease_epoch`
- lease-bound active mutation validation
- lease renew
- expiry observability
- reclaim creates a new attempt
- revoke for operator safety/drain
- partition-local Valkey indexes
- lease history

Designed for later but not required in v1:

- cooperative atomic handoff
- richer per-class lease-duration policy beyond execution/lane defaults
- selective relaxation for progress/stream appends without full lease validation

## Open Questions

Only unresolved questions remain here. Locked decisions from the pre-RFC are not reopened.

1. Should lease-duration policy resolve primarily from execution policy, lane defaults, or a merged policy with hard caps?
2. Should `lease_revoked` render publicly as a distinct operator-facing state, or remain an ownership-state nuance under the broader execution public state?

**Resolved:** Stream-frame appends require full lease validation (lease_id + lease_epoch + expiry check) in v1. See RFC-006 `append_frame.lua` which validates all three fields before every XADD.

## References

- Pre-RFC: `flowfabric_use_cases_and_primitives (2).md`
- Related RFCs: RFC-001, RFC-002, RFC-004
