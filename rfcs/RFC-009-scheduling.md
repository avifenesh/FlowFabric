# RFC-009: Scheduling, Fairness, and Admission

**Status:** Draft
**Author:** Worker-2 (Client & Public API Specialist)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitives 8 (Capability), 9 (Route), 10 (Submission/Queue facade), Section 4.4 (Scheduling and admission), Finalization §12 (fairness in v1)

---

## Summary

This RFC defines the **scheduling and admission layer** — the control plane that decides which eligible execution gets claimed next, where it runs, and under what fairness and admission constraints. It also defines the **lane** (queue facade), **capability matching**, **route resolution**, **priority model**, **fairness policy**, and the **claim-grant** mechanism that bridges the scheduler with the atomic lease acquisition in RFC-003. The scheduler is not a first-class durable object — it is a system-level function that reads execution state, worker registration, lane policy, and resource signals to produce claim-grant decisions.

## Motivation

FlowFabric must answer more than "which execution is next?" It must answer:

- Does this execution's capability requirements match any available worker?
- Is this tenant's work being starved by another tenant's burst?
- Is this lane paused, draining, or disabled?
- Does this execution have budget and quota headroom?
- Should this execution wait for a worker in a specific locality?
- How does priority interact with fairness?

Without an explicit scheduling/admission model, these questions are answered ad-hoc inside claim scripts, producing inconsistent behavior and undebuggable routing.

**Driving use cases:** UC-05 (priority execution), UC-09 (batched execution), UC-10 (queue-compatible mode), UC-15 (pause/drain/freeze), UC-49 (capability-based routing), UC-50 (tenant-aware routing), UC-51 (model-tier routing), UC-52 (isolation/sandbox routing), UC-53 (locality/region affinity), UC-54 (fair scheduling across groups).

---

## Detailed Design

### 1. Scheduling/Admission Layer Purpose

The scheduling layer sits between execution state and lease acquisition. It does not own durable state — it reads from execution records, worker registrations, lane configs, and resource policy, then produces a **claim-grant** that the atomic `acquire_lease` script (RFC-003) consumes.

```
┌─────────────┐     ┌────────────────────┐     ┌──────────────────┐
│ Worker asks  │────►│ Scheduling/Admission│────►│ Claim Grant      │
│ for work     │     │ Layer              │     │ (partition-local) │
└─────────────┘     └────────┬───────────┘     └────────┬─────────┘
                             │ reads:                    │
                    ┌────────┼────────────┐     ┌────────▼─────────┐
                    │ - eligible execs    │     │ acquire_lease.lua │
                    │ - worker caps       │     │ (RFC-003, atomic) │
                    │ - lane config       │     └──────────────────┘
                    │ - budget/quota      │
                    │ - fairness state    │
                    └─────────────────────┘
```

**Key separation:** The scheduler computes *what* to claim. The Valkey Function ensures *atomicity*. This avoids cross-slot reads inside atomic functions while keeping the final claim decision safe.

**Three-partition routing:** A single claim decision may touch three different Valkey Cluster shards via sequential `FCALL` invocations:

1. **Quota check** on `{q:K}` partition (RFC-008) — scope-level sliding-window rate check + concurrency check (fast pre-check)
2. **Candidate selection** (scheduler-local) — fairness, priority, capability match
3. **Budget check** on `{b:M}` partition (RFC-008) — per-candidate remaining allowance check against all attached budgets. If denied: block candidate via `block_execution_for_admission` on `{p:N}`, return to step 2 with next candidate.
4. **Claim-grant issuance** on `{p:N}` partition (this RFC) — validates execution eligibility, writes grant, removes from eligible set

If step 1 rejects, the scheduler stops (scope-level denial). If step 3 denies the candidate, the scheduler blocks that candidate and tries the next (per-candidate budget check). The `ff_claim_execution` function (RFC-001/003) subsequently runs on the same `{p:N}` partition as step 4, consuming the grant. Budget and quota are NOT re-validated inside the atomic claim function — they were checked by the scheduler before the grant was issued.

### 2. Scheduling Inputs

| Input | Source | Purpose |
|-------|--------|---------|
| `eligibility_state` | Execution core (RFC-001) | Only `eligible_now` executions can be claimed. |
| `public_state` | Execution core (RFC-001) | Must be `waiting`. Delayed, rate_limited, waiting_children are not claimable. |
| `priority` | Execution core (RFC-001) | Higher value = claimed first within a lane. |
| `lane_id` | Execution core (RFC-001) | Determines which lane's policies apply. |
| `lane_state` | Lane config | `intake_open`, `intake_paused`, `draining`, `disabled`. |
| `routing_requirements` | Execution policy (RFC-001) | Required/preferred/forbidden capabilities. |
| `worker_capabilities` | Worker registration | What the claimant can do. |
| `worker_capacity` | Worker registration | Current load, remaining slots. |
| `budget_remaining` | Budget system (RFC-008) | Whether budget allows a new claim. |
| `quota_status` | Quota/rate-limit system (RFC-008) | Whether admission window allows a new claim. |
| `tenant_id` / `namespace` | Execution core (RFC-001) | For cross-tenant fairness. |
| `delay_until` | Execution core (RFC-001) | Not claimable until this time passes. |
| `retry_backoff` | Execution policy | Timing of retry eligibility. |
| `flow_dependencies` | Flow system (RFC-007) | Whether upstream dependencies are satisfied. |

### 3. Claim-Grant Model

The claim-grant is the bridge between the scheduling layer (multi-key reads, cross-partition logic) and the atomic lease acquisition script (single-partition, single-script).

#### 3.1 How It Works

1. **Worker requests work:** Sends `claim_request(worker_id, capabilities, preferred_lanes?, batch_size?)` to the scheduling layer.
2. **Quota pre-check (scope-level):** Scheduler checks quota/rate-limit admission on `{q:K}`. If denied, return rejection to worker. This is a scope-level fast pre-check — it does not select a specific execution.
3. **Select candidate:** Scheduler reads eligible executions from partition-local sorted sets, checks capability match, applies fairness policy (cross-lane weighted round-robin, cross-tenant fair share), and selects the highest-priority candidate.
4. **Budget check (per-candidate):** Scheduler reads attached budget usage on `{b:M}` for the selected candidate. If any hard-limit budget is exhausted: block this candidate via `block_execution_for_admission` on `{p:N}`, then go back to step 3 with the next candidate. If no more candidates, return empty response to worker.
5. **Scheduler issues grant:** Writes a short-lived `claim_grant` key to the target partition with the worker's identity, capabilities hash, and a grant expiry.
6. **Worker (or scheduler) calls `acquire_lease`:** The atomic Valkey Function `ff_claim_execution` (RFC-001/003) validates the grant, creates the lease + attempt, and transitions execution to `active`. If the grant expired or another worker already claimed, the function rejects.

#### 3.2 Claim Grant Object

```
Key: ff:exec:{p:N}:<execution_id>:claim_grant
Type: Hash
TTL: short (e.g. 5 seconds)
```

| Field | Type | Description |
|-------|------|-------------|
| `worker_id` | `String` | Logical worker identity. |
| `worker_instance_id` | `String` | Concrete process/container identity. |
| `lane_id` | `String` | Lane the grant was issued from. |
| `capability_hash` | `String` | Hash of the capability snapshot that justified the match. |
| `grant_expires_at` | `Timestamp` | Hard deadline to consume the grant. |
| `route_snapshot_json` | `String` | Compact route decision (selected pool, locality, reason). |
| `admission_summary` | `String` | Human-readable summary of admission context (e.g. "budget 45% used, quota 8/100 RPM"). |

#### 3.3 Grant Invariants

| ID | Rule |
|----|------|
| CG1 | A claim-grant is valid for exactly one execution and one worker. |
| CG2 | A grant expires if not consumed within the TTL. The execution returns to the eligible pool. |
| CG3 | `acquire_lease` must validate the grant atomically — if the grant is missing, expired, or mismatched, the claim is rejected. |
| CG4 | Only one grant may exist per execution at a time. Issuing a new grant for the same execution implicitly invalidates the prior one (key overwrite). |

---

### 4. Priority Model

#### 4.1 Per-Lane Priority Ordering

Within a lane, executions are ordered by priority. Higher priority value = claimed first. Equal priority is broken by creation timestamp (FIFO within same priority tier).

The eligible sorted set uses a composite score:

```
score = -(priority * 1_000_000_000_000) + created_at_ms
```

This ensures `ZPOPMIN` (or `ZRANGEBYSCORE ... LIMIT 0 1`) always returns the highest-priority, oldest execution first.

#### 4.2 Priority Field

| Field | Type | Range | Default | Description |
|-------|------|-------|---------|-------------|
| `priority` | `i32` | `-2^31` to `2^31-1` | `0` | Higher = more urgent. Negative for background work. |

Priority may be set at submission time or changed later via `change_priority` (RFC-001 §4.4). The sorted set score must be updated atomically with the priority change.

#### 4.3 Priority Starvation Prevention

Pure priority ordering can starve low-priority work. The fairness policy (§5) provides cross-lane and cross-tenant protection, but within a single lane, starvation of low-priority work is accepted as a design choice — the operator chose those priorities.

If intra-lane starvation becomes a v2 concern, aging (gradually boosting score of long-waiting executions) can be added without changing the sorted-set model.

**Operator warning:** Lanes with mixed priorities under continuous high-priority submission will starve low-priority work indefinitely. Mitigation options: (a) use separate lanes for different priority tiers, (b) set a maximum priority spread within a lane via lane policy, (c) wait for v2 aging support.

---

### 5. Fairness Policy

**Locked decision:** Cross-lane and cross-tenant fairness are v1 concerns. V1 does not require a heavyweight global scheduler, but it does require more than purely lane-local priority.

#### 5.1 Fairness Scope

| Scope | Mechanism | Description |
|-------|-----------|-------------|
| **Cross-lane** | Weighted round-robin | When a worker can claim from multiple lanes, the scheduler selects which lane to serve next using weights. Prevents one hot lane from starving others. |
| **Cross-tenant** | Tenant-weighted fair share | When multiple tenants share a lane or worker pool, the scheduler ensures no single tenant monopolizes claim bandwidth. |
| **Within-lane** | Priority ordering | Priority is the primary ordering. Fairness is not applied within a single lane — the priority ordering is the operator's intent. |

#### 5.2 Weighted Round-Robin Across Lanes

The scheduler maintains a per-worker-pool **lane selection cursor** that cycles through eligible lanes weighted by their scheduling weight.

| Field | Type | Description |
|-------|------|-------------|
| `lane_id` | `String` | The lane. |
| `scheduling_weight` | `u32` | Relative weight (default: 1). Higher weight = more claims served per cycle. |
| `last_served_at` | `Timestamp` | When this lane was last served by this pool. Used for deficit tracking. |
| `deficit` | `f64` | Running deficit for weighted fair queuing. Reset periodically. |

Algorithm (simplified weighted deficit round-robin):

```
on claim_request(worker):
    eligible_lanes = lanes where:
        lane.state == intake_open or draining
        lane has eligible executions matching worker capabilities
    
    select lane with highest deficit (most underserved)
    serve one execution from that lane
    reduce that lane's deficit by 1.0 / lane.scheduling_weight
    clamp: lane.deficit = max(lane.deficit, -(2.0 * lane.scheduling_weight))
```

#### 5.3 Cross-Tenant Fairness

When multiple tenants (namespaces) share a lane, the scheduler applies a **tenant fair-share** policy:

| Field | Type | Description |
|-------|------|-------------|
| `namespace` | `String` | Tenant identity. |
| `tenant_weight` | `u32` | Relative claim share (default: 1). |
| `claims_in_window` | `u32` | Claims served in the current fairness window. |
| `window_start` | `Timestamp` | Start of current fairness window. |
| `window_duration_ms` | `u64` | Fairness evaluation window (e.g. 10 seconds). |

Within a lane, if the next candidate by priority belongs to a tenant that has consumed more than its fair share in the current window, the scheduler may skip it in favor of an underserved tenant's execution (subject to priority floor — a high-priority execution is never skipped below a configurable threshold).

**Deficit cap (symmetric):** Deficit per tenant and per lane is clamped to the range `[-(2 × weight), +(2 × weight)]`:
- **Positive cap** (`+2 × weight`): Prevents runaway rebalancing after extended budget exhaustion or capacity outage. Without this cap, a tenant blocked by budget for hours would accumulate massive deficit that monopolizes claims for an extended period once the budget frees.
- **Negative cap** (`-(2 × weight)`): Prevents unbounded recovery time after a burst. Without this cap, a lane that served 50K executions during a burst accumulates deficit of -50K. After the burst ends, the lane would need 50K scheduling cycles before it receives fair treatment again — even if its backlog is long cleared. The negative cap bounds recovery to at most `4 × weight` cycles (from `-2w` to `+2w`).

#### 5.4 V1 Fairness Ignores Flow/Coordinator Priority

V1 fairness operates at the lane and tenant level only. It does not consider flow-level priority, coordinator urgency, or DAG critical-path information. A flow member execution is scheduled by its own `priority` field and its lane/tenant fairness context — the scheduler has no concept of "this execution is on the critical path of a high-priority flow."

Flow-aware priority boosting (e.g. inheriting the parent flow's priority, or boosting DAG critical-path nodes) is a designed-for extension that requires the scheduler to read flow metadata, which adds cross-partition complexity.

#### 5.5 Fairness Is Best-Effort

Fairness is computed by the scheduling layer, not enforced by the atomic Valkey Function. This means:

- Under low contention, fairness has minimal effect — executions are claimed as they arrive.
- Under high contention, fairness shapes claim distribution across lanes/tenants.
- A determined adversary with a massive burst can temporarily skew fairness before the scheduler rebalances. This is acceptable for v1.

Fairness state (deficits, window counters) is stored in Valkey as ephemeral keys with short TTL. It does not need to survive restarts — the scheduler rebuilds fair state from current queue depths on restart.

---

### 6. Route Resolution

Route resolution determines *where* an execution should run by matching execution requirements against available workers.

#### 6.1 Route Object

A resolved route is a snapshot of the placement decision. It is stored on the attempt's policy snapshot (RFC-002 `AttemptPolicySnapshot.route_snapshot`) and on the claim-grant.

| Field | Type | Description |
|-------|------|-------------|
| `route_id` | `UUID` | Unique identifier for this route decision. |
| `lane_id` | `String` | Lane the execution was routed through. |
| `worker_pool` | `String` | Target worker pool, if applicable. |
| `locality` | `String` | Selected region/zone, if applicable. |
| `capability_profile` | `Vec<String>` | Capabilities that were matched. |
| `route_reason` | `String` | Human-readable explanation of why this route was chosen. |
| `route_version` | `u64` | Monotonic version for tracking reroutes. |
| `resolved_at` | `Timestamp` | When the route was computed. |

#### 6.2 Route Outcomes

| Outcome | Maps To (RFC-001) | Description |
|---------|-------------------|-------------|
| `routable_now` | `eligible_now` | A matching worker exists and can claim. |
| `waiting_for_capable_worker` | `blocked_by_route` + `waiting_for_capable_worker` | Requirements exist but no registered worker satisfies them. |
| `waiting_for_capacity` | `blocked_by_route` + `waiting_for_capable_worker` | Capable workers exist but all are at capacity. |
| `waiting_for_locality_match` | `blocked_by_route` + `waiting_for_locality_match` | Requires a specific locality but no worker there is available. |
| `waiting_for_quota` | `blocked_by_quota` + `waiting_for_quota` | Quota/rate-limit prevents admission. |
| `reroute_required` | — | Current route is invalid (worker pool gone, capability changed). Scheduler must re-resolve. |
| `unroutable_by_policy` | `blocked_by_operator` + `paused_by_policy` | Policy forbids routing (lane disabled, compliance block). |

#### 6.3 Reroute Rules

Reroute is allowed when:
- Execution is not actively leased (in `runnable` state)
- Retry policy changes the target (fallback progression)
- Operator explicitly reroutes
- Worker pool availability changed

Reroute increments `route_version` and is recorded in the execution's audit trail.

---

### 7. Capability Matching

#### 7.1 Worker Registration

Workers register their capabilities on startup and periodically refresh them.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `worker_id` | `String` | yes | Logical worker identity (e.g. `"inference-pool-gpu-1"`). |
| `worker_instance_id` | `String` | yes | Concrete process/container identity. |
| `capabilities` | `Map<String, String>` | yes | Key-value capability attributes (e.g. `{"provider": "anthropic", "model_family": "claude", "region": "us-east-1", "gpu": "a100"}`). |
| `registered_at` | `Timestamp` | yes | Initial registration time. |
| `last_heartbeat_at` | `Timestamp` | yes | Last liveness signal. |
| `max_concurrent_claims` | `u32` | no | Capacity limit. Default: 1. |
| `current_claim_count` | `u32` | no | Current active leases held. |
| `status` | `WorkerStatus` | yes | `active`, `draining`, `unhealthy`, `deregistered`. |
| `metadata` | `Map<String, String>` | no | Arbitrary metadata (version, build, etc.). |

Workers with `status != active` are excluded from capability matching.

#### 7.2 Execution Routing Requirements

Set on the execution's policy at submission time.

| Field | Type | Description |
|-------|------|-------------|
| `required_capabilities` | `Map<String, String>` | Must all be present on the worker. |
| `preferred_capabilities` | `Map<String, String>` | Influence selection but not mandatory. |
| `forbidden_capabilities` | `Map<String, String>` | Must NOT be present on the worker. |
| `min_isolation_level` | `String` | Minimum isolation/sandbox tier (e.g. `"standard"`, `"sandbox"`, `"airgap"`). |
| `locality_hint` | `String` | Preferred region/zone. Not mandatory unless `locality_required = true`. |
| `locality_required` | `bool` | If true, locality_hint becomes a hard constraint. |

#### 7.3 Matching Algorithm

```
fn matches(worker: &WorkerCaps, exec: &RoutingRequirements) -> MatchResult {
    // 1. Required capabilities must all be satisfied
    for (key, value) in exec.required_capabilities:
        if worker.capabilities.get(key) != Some(value):
            return Ineligible(reason: "missing required: {key}={value}")

    // 2. Forbidden capabilities must not be present
    for (key, value) in exec.forbidden_capabilities:
        if worker.capabilities.get(key) == Some(value):
            return Ineligible(reason: "forbidden present: {key}={value}")

    // 3. Isolation level check
    if worker.isolation_level < exec.min_isolation_level:
        return Ineligible(reason: "insufficient isolation")

    // 4. Locality check
    if exec.locality_required && worker.locality != exec.locality_hint:
        return Ineligible(reason: "locality mismatch")

    // 5. Capacity check
    if worker.current_claim_count >= worker.max_concurrent_claims:
        return NoCapacity

    // 6. Score by preferences (higher = better match)
    let score = count_matching_preferences(worker, exec)
    return Eligible(score)
}
```

The scheduler prefers the highest-scoring eligible worker. Ties are broken by least-loaded (fewest current claims).

#### 7.4 Capability Invariants

| ID | Invariant | Rationale |
|----|-----------|-----------|
| C1 | **Requirements must be explicit.** Execution requirements are encoded on the execution, not inferred from lane name. | Prevents implicit routing that breaks when lanes are reconfigured. |
| C2 | **Ineligible workers must not claim.** The claim-grant is only issued for workers that pass matching. The `ff_claim_execution` function validates the capability hash. | Prevents misrouted work. |
| C3 | **Capability view is inspectable.** `explain_capability_mismatch` must show why no worker matches. | "Why is this stuck?" for routing. |
| C4 | **Matching is deterministic enough to debug.** Same inputs → same match result. | Operators can reproduce routing decisions. |

---

### 8. Lane / Queue Facade

A lane is a **named submission surface with default policies** that creates executions. It is the queue-compatible ingress for teams migrating from BullMQ-style systems.

**Invariant Q1:** Every queued item maps to an execution. There is no parallel shadow "job model" with different semantics.

**Invariant Q2:** Lane configuration provides policy *defaults*, not separate state rules.

**Invariant Q3:** Lane operations cannot bypass core invariants (lease, suspension, flow, routing, budget).

#### 8.1 Lane Object

```
Key: ff:lane:{lane_id}:config  →  HASH
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `lane_id` | `String` | yes | Unique lane identifier. |
| `namespace` | `String` | yes | Owning tenant/workspace. |
| `display_name` | `String` | no | Human-readable name. |
| `state` | `LaneState` | yes | `intake_open`, `intake_paused`, `draining`, `disabled`. |
| `default_priority` | `i32` | no | Default priority for submissions without explicit priority. Default: `0`. |
| `default_retry_policy` | `RetryPolicy` | no | Default retry policy for executions created in this lane. |
| `default_timeout_policy` | `TimeoutPolicy` | no | Default per-attempt timeout. |
| `default_routing_requirements` | `RoutingRequirements` | no | Default capability/locality requirements. |
| `default_budget_ids` | `Vec<UUID>` | no | Default budget attachments. |
| `default_stream_policy` | `StreamPolicy` | no | Default stream durability mode. |
| `scheduling_weight` | `u32` | no | Relative weight for cross-lane fairness. Default: `1`. |
| `max_concurrency` | `u32` | no | Max simultaneous active executions from this lane. `0` = unlimited. |
| `created_at` | `Timestamp` | yes | When the lane was created. |
| `updated_at` | `Timestamp` | yes | Last config change. |

#### 8.2 Lane State Model

```
intake_open ──► intake_paused ──► draining ──► disabled
     ▲               │                             │
     └───────────────┘                             │
     ▲                                             │
     └─────────────────────────────────────────────┘
```

| State | Submissions | Claims | Active Work |
|-------|------------|--------|-------------|
| `intake_open` | Accepted | Allowed | Continues |
| `intake_paused` | Rejected | Allowed — all eligible work including retries, reclaims, and resumes of existing executions. Only new `enqueue` submissions are blocked. | Continues |
| `draining` | Rejected | Allowed (drain remaining) | Continues until empty |
| `disabled` | Rejected | Blocked | Active work continues but no new claims |

**Execution eligibility effect:** When a lane is `intake_paused` or `draining`, existing eligible executions remain claimable. When `disabled`, the scheduler sets `eligibility_state = blocked_by_lane_state` and `blocking_reason = paused_by_policy` on un-claimed executions.

#### 8.3 Submission Modes

| Mode | Operation | Description |
|------|-----------|-------------|
| Fire-and-forget | `enqueue(lane, payload, opts)` | Create execution, return execution_id immediately. |
| Delayed | `enqueue_delayed(lane, payload, delay_until)` | Create execution with `eligibility_state = not_eligible_until_time`. |
| Scheduled | `enqueue_scheduled(lane, payload, schedule_spec)` | **V1:** creates a single delayed execution with `delay_until` from `schedule_spec`. Recurring creation from a cron/interval schedule requires the schedule loop (deferred — see RFC-001 V1 scope). Callers needing recurring execution in v1 must submit new executions externally (e.g., external cron calling `enqueue`). |
| Request/reply | `enqueue_and_wait(lane, payload, wait_policy)` | Create execution + block caller until completion or timeout. Implemented as: create execution → subscribe to completion event → return result. |
| Bulk | `enqueue_bulk(lane, items)` | Create multiple executions in one call. Individual idempotency per item. |
| Deduplicated | `enqueue(lane, payload, idempotency_key)` | Standard enqueue with idempotency_key. Returns existing execution if key matches within dedup window. |

All modes create standard execution objects (RFC-001). The mode only affects the submission API ergonomics and initial state.

#### 8.4 Bulk Submission Implementation

`enqueue_bulk` is implemented as **N independent `create_execution` calls**, one per item. Each execution is created on its own `{p:N}` partition (determined by `hash(execution_id) % num_partitions`). Different items in the batch will generally land on different partitions — cross-partition atomicity is impossible in Valkey Cluster.

**Partial failure:** If the caller crashes or a connection drops after K of N items succeed, the first K executions are in valid, independently claimable states. The remaining N-K do not exist. On retry, the caller resubmits the full batch. Items that already exist are caught by their per-item `idempotency_key` (`SET NX` returns the existing `execution_id`). Items that failed are created fresh.

**Ordering:** No ordering guarantee across items in a batch. Items may be created in any order and on any partition. If ordering matters, use explicit dependencies (RFC-007 chain flow).

#### 8.5 Request/Reply Implementation (V1)

`enqueue_and_wait` blocks the caller until the execution completes or a timeout elapses. V1 implementation:

1. Caller creates execution via `create_execution` (same as fire-and-forget).
2. Caller issues `XREAD BLOCK <wait_timeout_ms> STREAMS ff:exec:{p:N}:<execution_id>:lease:history $` on the execution's partition.
3. When `complete_execution` or `fail_execution` fires, it XADDs a `released` event to the lease history stream with `reason = completed` or `reason = failed_terminal`.
4. Caller receives the event, reads the result payload from `ff:exec:{p:N}:<execution_id>:result`.
5. If `XREAD BLOCK` times out before a completion event, caller returns a timeout error to the upstream.

**Why lease_history:** The lease history stream already receives events on every lease-releasing transition (complete, fail, suspend). No additional key or pub/sub infrastructure is needed. The caller filters for `reason = completed` or `reason = failed_terminal`.

**Limitation:** The caller must know the execution's partition to construct the stream key. The `create_execution` response includes `partition_id`, which the caller uses to derive the `{p:N}` prefix.

**Replay caveat:** On receiving a completion event (`reason = completed` or `reason = failed_terminal`), the caller MUST immediately stop tailing `lease_history` and read the result. Subsequent events in the stream (e.g., `replayed`, `acquired` from a replay or reclaim) belong to a new lifecycle and must not be consumed by the original `enqueue_and_wait` caller. A naive implementation that continues XREAD after seeing completion would observe confusing state transitions (completed → replayed → acquired). The correct pattern: filter for the first completion event, return the result, close the XREAD.

---

### 9. Lane State Model

See §8.2. Lane state transitions are explicit control operations, not automatic:

| Operation | From | To | Effect |
|-----------|------|----|--------|
| `pause_lane` | `intake_open` | `intake_paused` | New submissions rejected. Existing eligible work still claimable. |
| `resume_lane` | `intake_paused` | `intake_open` | Submissions accepted again. |
| `drain_lane` | `intake_open` or `intake_paused` | `draining` | No new submissions. Existing work claims continue until lane is empty. |
| `disable_lane` | Any | `disabled` | No submissions, no new claims. Active leases continue. |
| `enable_lane` | `disabled` | `intake_open` | Full operation restored. Blocked executions re-evaluated for eligibility. |

---

### 10. Queue-Facing Operations

#### 10.1 Submission Operations

| Operation | Class | Parameters | Semantics |
|-----------|-------|-----------|-----------|
| `enqueue` | A | `lane_id`, `execution_kind`, `payload`, `priority?`, `idempotency_key?`, `tags?`, `routing_requirements?` | Creates execution with lane defaults merged. Returns `execution_id`. |
| `enqueue_delayed` | A | Same + `delay_until` | Creates with `eligibility_state = not_eligible_until_time`. |
| `enqueue_scheduled` | A | Same + `schedule_spec` | Creates with schedule metadata. |
| `enqueue_and_wait` | A+C | Same + `wait_timeout_ms` | Creates execution, then blocks on completion notification. Returns result or timeout error. |
| `enqueue_bulk` | A | `lane_id`, `Vec<item>` | Creates multiple executions. Returns `Vec<execution_id>`. Individual idempotency per item. |

#### 10.2 Lane Control Operations

| Operation | Class | Parameters | Semantics |
|-----------|-------|-----------|-----------|
| `pause_lane` | A | `lane_id`, `reason` | Sets state to `intake_paused`. |
| `resume_lane` | A | `lane_id` | Sets state to `intake_open`. |
| `drain_lane` | A | `lane_id` | Sets state to `draining`. |
| `disable_lane` | A | `lane_id`, `reason` | Sets state to `disabled`. Updates eligibility on unclaimed executions. |
| `enable_lane` | A | `lane_id` | Sets state to `intake_open`. Re-evaluates blocked executions. |
| `get_lane_counts` | C | `lane_id` | Returns counts by public_state: waiting, delayed, active, suspended, completed, failed, etc. |
| `get_lane_metrics` | C | `lane_id` | Returns throughput, avg latency, backlog depth, retry rate, suspension rate. |
| `delete_lane` | A | `lane_id` | **Preconditions:** state must be `disabled` AND no non-terminal executions exist in any partition sorted set for this lane (eligible, delayed, active, suspended, blocked sets all empty). Deletes `ff:lane:<lane_id>:config` and `ff:lane:<lane_id>:counts`. Per-partition empty sorted sets are cleaned up lazily (empty ZSETs are negligible). Executions that referenced the deleted lane retain `lane_id` on their core hash as metadata — terminal executions are purged by the retention scanner normally. |

#### 10.3 Worker Registration Operations

| Operation | Class | Parameters | Semantics |
|-----------|-------|-----------|-----------|
| `register_worker` | A | `worker_id`, `worker_instance_id`, `capabilities`, `max_concurrent_claims?` | Creates or updates worker registration. |
| `heartbeat_worker` | B | `worker_instance_id` | Updates `last_heartbeat_at`. Keeps worker alive for scheduling. |
| `update_worker_capabilities` | A | `worker_instance_id`, `capabilities` | Updates capability snapshot. |
| `deregister_worker` | A | `worker_instance_id` | Sets status to `deregistered`. Active leases are NOT revoked (handled by lease expiry/drain). |
| `get_worker` | C | `worker_instance_id` | Returns full worker registration. |
| `list_workers` | C | `status?`, `capability_filter?` | Returns matching workers. |
| `explain_capability_mismatch` | C | `execution_id` | Returns a structured explanation of why no registered worker matches the execution's requirements. Lists each requirement and which workers fail on which criterion. |

---

### 11. Admission Rejection Model

When the scheduler cannot issue a claim-grant, it must provide an **explainable rejection** that maps to RFC-001's blocking model.

| Rejection Reason | Execution Effect | Blocking Reason (RFC-001) |
|-----------------|-----------------|---------------------------|
| No eligible execution in requested lanes | N/A (worker gets empty response) | — |
| Lane is paused/disabled | `eligibility_state = blocked_by_lane_state` | `paused_by_policy` |
| No capable worker registered | `eligibility_state = blocked_by_route` | `waiting_for_capable_worker` |
| Capable workers exist but all at capacity | `eligibility_state = blocked_by_route` | `waiting_for_capable_worker` |
| Locality requirement unsatisfied | `eligibility_state = blocked_by_route` | `waiting_for_locality_match` |
| Budget exhausted | `eligibility_state = blocked_by_budget` | `waiting_for_budget` |
| Quota/rate-limit window full | `eligibility_state = blocked_by_quota` | `waiting_for_quota` |
| Operator hold | `eligibility_state = blocked_by_operator` | `paused_by_operator` |
| Tenant fair-share exceeded (temporary) | No state change — execution remains eligible but skipped in this cycle | — (scheduling concern, not execution state) |

**Important:** Admission rejection due to fairness does NOT change execution state. The execution remains `eligible_now` — it is simply passed over in this scheduling cycle. Only structural blocks (no worker, no budget, lane paused) update the execution's eligibility state.

**Critical implementation note for structural blocks:** When the scheduler denies a claim due to budget, quota, route, or lane state, it MUST atomically update the execution's state vector AND move it between sorted sets. Otherwise the execution remains in the eligible set with `eligible_now` but is unclaimed — appearing as "waiting for worker" when the real cause is budget/quota/route. Specifically:

- **Budget denial:** Run `ff_block_execution_for_admission` on `{p:N}` that sets `eligibility_state = blocked_by_budget`, `blocking_reason = waiting_for_budget`, `public_state = rate_limited`, ZREMs from `ff:idx:{p:N}:lane:<lane_id>:eligible`, ZADDs to `ff:idx:{p:N}:lane:<lane_id>:blocked:budget`. All 7 state vector dimensions must be set.
- **Quota denial:** Same pattern with `blocked_by_quota`, `waiting_for_quota`, ZADD to `blocked:quota`.
- **Route denial:** Same pattern with `blocked_by_route`, `waiting_for_capable_worker` or `waiting_for_locality_match`, ZADD to `blocked:route`.
- **Lane state denial:** Same pattern with `blocked_by_lane_state`, `paused_by_policy`, ZADD to `blocked:operator`.

When the blocking condition clears (budget increased, quota window resets, worker registers, lane unpaused), a corresponding **unblock scanner** must detect the condition change and move executions back from the blocked set to the eligible set. This is analogous to the delayed→eligible promoter but triggered by resource availability changes rather than time.

**Batch-block short-circuit for repeated rejections:** When the scheduler iterates an eligible set and encounters N consecutive capability mismatches (default: 10), it MUST stop scanning and batch-block the rejected candidates instead of continuing through the entire set. Without this, a lane with 50K unmatchable executions causes a hot loop where the scheduler repeatedly scans and rejects without making progress.

Implementation:
1. Scanner maintains a consecutive-rejection counter per lane per cycle.
2. When the counter reaches the batch-block threshold (default: 10), the scheduler stops scanning this lane for this cycle.
3. It issues a single batch `ff_block_execution_for_admission_batch` call per partition that blocks up to 100 executions from the eligible set head. The function iterates `ZRANGE eligible 0 99`, checks each execution's routing requirements against the rejection reason, and ZREMs + ZADDs in bulk.
4. A separate batch variant `ff_block_execution_for_admission_batch` (script 29a-batch) is required for this:

```lua
-- KEYS: eligible_zset, target_blocked_zset, exec_core_1..exec_core_N
-- ARGV: block_eligibility_state, block_blocking_reason, block_blocking_detail,
--       block_public_state, now_ms, execution_id_1..execution_id_N
-- Processes up to N executions in one FCALL (all on same {p:N}).
-- For each: validate still eligible_now + runnable, HSET 7 dims, ZREM eligible, ZADD blocked.
-- Returns count of actually blocked executions.
```

5. The scheduler resumes scanning other lanes normally. The blocked executions will be unblocked by the eligibility re-evaluation scanner (§6.15 in RFC-010) when capable workers register.

---

### 12. Valkey Data Model

#### 12.1 Lane Configuration

```
ff:lane:<lane_id>:config  →  HASH
  lane_id              → inference-fast
  namespace            → tenant-abc
  state                → intake_open
  default_priority     → 0
  default_retry_json   → {"max_attempts": 3, "backoff": "exponential"}
  default_timeout_ms   → 30000
  default_routing_json → {"required": {"provider": "anthropic"}}
  default_budget_ids   → budget-uuid-1,budget-uuid-2
  scheduling_weight    → 2
  max_concurrency      → 0
  created_at           → 1713100800000
  updated_at           → 1713100800000
```

Lane config is a global key (not partitioned) — there are few lanes and they are read frequently. Config changes are infrequent.

**Lane registry:**

```
ff:idx:lanes  →  SET
  member: lane_id
```

SADD on `create_lane`, SREM on `delete_lane`. The scheduler reads this set at startup and refreshes periodically (every 60s) to discover new or removed lanes. Required for cross-lane fairness round-robin.

#### 12.2 Per-Partition Eligible Sorted Sets

Already defined in RFC-001 §9.3. The scheduler reads from these:

```
ff:idx:{p:N}:lane:<lane_id>:eligible  →  ZSET
  member: execution_id
  score:  -(priority * 1_000_000_000_000) + created_at_ms
```

The scheduler scans eligible sets across partitions for the requested lanes, applies fairness policy, and selects the best candidate.

#### 12.3 Worker Registration

```
ff:worker:<worker_instance_id>  →  HASH
  worker_id              → inference-pool-gpu-1
  worker_instance_id     → i-0abc123def
  capabilities_json      → {"provider": "anthropic", "model_family": "claude", ...}
  capability_hash        → sha256:abc123...
  registered_at          → 1713100800000
  last_heartbeat_at      → 1713100900000
  max_concurrent_claims  → 4
  current_claim_count    → 2
  status                 → active
  metadata_json          → {"version": "1.2.0"}
```

TTL: `last_heartbeat_at + worker_ttl_ms`. Workers that stop heartbeating expire automatically. The scheduler skips workers with `status != active` or expired heartbeats.

Worker capability index (for fast lookup by capability):

```
ff:idx:workers:cap:<key>:<value>  →  SET
  member: worker_instance_id
```

Example: `ff:idx:workers:cap:provider:anthropic` → `{i-0abc123def, i-0xyz456ghi}`.

Updated atomically with `register_worker` / `update_worker_capabilities`.

#### 12.4 Claim-Grant Keys

Already defined in RFC-003 and §3.2 above:

```
ff:exec:{p:N}:<execution_id>:claim_grant  →  HASH
  worker_id, worker_instance_id, lane_id,
  capability_hash, grant_expires_at,
  route_snapshot_json, admission_summary
TTL: 5 seconds
```

#### 12.5 Fairness State (Ephemeral)

```
ff:sched:fairness:lane:<lane_id>:deficit  →  STRING
  value: float deficit counter
  TTL: fairness_window_ms * 2
```

```
ff:sched:fairness:tenant:<namespace>:lane:<lane_id>:claims  →  STRING
  value: claim count in current window
  TTL: fairness_window_ms
```

These are ephemeral. On scheduler restart, deficits reset to zero and rebuild naturally from current queue depths.

#### 12.6 Lane Counts (Derived, Cached)

```
ff:lane:<lane_id>:counts  →  HASH
  waiting    → 142
  delayed    → 23
  active     → 56
  suspended  → 7
  completed  → 10234
  failed     → 89
  cancelled  → 12
```

Updated periodically by a background aggregator that scans partition-local sorted sets. Not authoritative — `ZCARD` on the sorted sets is authoritative but slower.

#### 12.7 Issue Claim-Grant Function

`ff_issue_claim_grant` (Valkey Function)

This script runs within a single partition. The scheduling layer calls it after selecting a candidate execution.

```lua
-- KEYS: exec_core, claim_grant_key, eligible_zset
-- ARGV: execution_id, worker_id, worker_instance_id, lane_id,
--       capability_hash, grant_ttl_ms, route_snapshot_json,
--       admission_summary, now_ms
-- NOTE: Budget/quota checked by scheduler before grant; not re-validated here.
--       Quota on {q:K}, budget on {b:M} — separate FCALL before this one.

local now_ms = tonumber(ARGV.now_ms)
local core = redis.call("HGETALL", KEYS[1])

-- 1. Validate execution is still eligible
if core.lifecycle_phase ~= "runnable" then return err("execution_not_eligible") end
if core.ownership_state ~= "unowned" then return err("execution_not_eligible") end
if core.eligibility_state ~= "eligible_now" then return err("execution_not_eligible") end
if core.terminal_outcome ~= "none" then return err("execution_not_eligible") end

-- 2. Verify no existing grant
if redis.call("EXISTS", KEYS[2]) == 1 then
  return err("grant_already_exists")
end

-- 3. Verify execution is in the eligible set
local rank = redis.call("ZRANK", KEYS[3], ARGV.execution_id)
if rank == nil then return err("execution_not_in_eligible_set") end

-- 4. Write claim grant with TTL
local expires_at = now_ms + tonumber(ARGV.grant_ttl_ms)
redis.call("HSET", KEYS[2],
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "lane_id", ARGV.lane_id,
  "capability_hash", ARGV.capability_hash,
  "grant_expires_at", expires_at,
  "route_snapshot_json", ARGV.route_snapshot_json,
  "admission_summary", ARGV.admission_summary)
redis.call("PEXPIRE", KEYS[2], tonumber(ARGV.grant_ttl_ms))

-- 5. Remove from eligible set (prevent double-grant)
redis.call("ZREM", KEYS[3], ARGV.execution_id)

return ok(expires_at)
```

**Note:** If the grant expires without being consumed, a background reconciler must re-add the execution to the eligible set (check execution state, re-add if still `runnable` + `eligible_now`).

#### 12.8 Reclaim Grant Script

`ff_issue_reclaim_grant` (Valkey Function)

Used by the reclaim path when a lease-expired or lease-revoked execution needs to be claimed by a new worker. Unlike `issue_claim_grant` which requires the execution to be in the eligible sorted set, this script validates reclaimable ownership state directly on the execution core.

```lua
-- KEYS: exec_core, claim_grant_key
-- ARGV: execution_id, worker_id, worker_instance_id, lane_id,
--       capability_hash, grant_ttl_ms, route_snapshot_json,
--       admission_summary, now_ms
-- NOTE: Budget/quota checked by scheduler before grant; not re-validated here.

local now_ms = tonumber(ARGV.now_ms)
local core = redis.call("HGETALL", KEYS[1])

-- 1. Validate execution is reclaimable (NOT eligible-set based)
if core.lifecycle_phase ~= "active" then return err("execution_not_reclaimable") end
local os = core.ownership_state
if os ~= "lease_expired_reclaimable" and os ~= "lease_revoked" then
  return err("execution_not_reclaimable")
end
if core.terminal_outcome ~= "none" then return err("execution_not_reclaimable") end

-- 2. Verify no existing grant
if redis.call("EXISTS", KEYS[2]) == 1 then
  return err("grant_already_exists")
end

-- 3. Write claim grant with TTL
local expires_at = now_ms + tonumber(ARGV.grant_ttl_ms)
redis.call("HSET", KEYS[2],
  "worker_id", ARGV.worker_id,
  "worker_instance_id", ARGV.worker_instance_id,
  "lane_id", ARGV.lane_id,
  "capability_hash", ARGV.capability_hash,
  "grant_expires_at", expires_at,
  "route_snapshot_json", ARGV.route_snapshot_json,
  "admission_summary", ARGV.admission_summary)
redis.call("PEXPIRE", KEYS[2], tonumber(ARGV.grant_ttl_ms))

-- No ZREM from eligible set — execution is not in the eligible set.
-- It's in the active set with expired/revoked ownership.

return ok(expires_at)
```

#### 12.9 Block Execution for Admission Script

`ff_block_execution_for_admission` (Valkey Function)

When the scheduler denies a claim due to a structural block (budget, quota, route, or lane state), it must atomically move the execution from the eligible set to the appropriate blocked set and update the state vector. Without this, the execution stays in the eligible set appearing as "waiting for worker" when the real cause is budget/quota/route.

```lua
-- KEYS: exec_core, eligible_zset, target_blocked_zset
-- ARGV: execution_id, block_eligibility_state, block_blocking_reason,
--       block_blocking_detail, block_public_state, now_ms
-- NOTE: Parameterized by block reason. Scheduler passes the specific
--       eligibility_state/blocking_reason/blocking_detail/public_state for the denial type.

local now_ms = tonumber(ARGV.now_ms)
local core = redis.call("HGETALL", KEYS[1])

-- Validate: only block if still eligible_now and runnable
if core.lifecycle_phase ~= "runnable" then return ok("not_runnable") end
if core.eligibility_state ~= "eligible_now" then return ok("already_blocked") end
if core.terminal_outcome ~= "none" then return ok("terminal") end

-- Update state vector — ALL 7 dimensions
redis.call("HSET", KEYS[1],
  "lifecycle_phase", "runnable",
  "ownership_state", "unowned",
  "eligibility_state", ARGV.block_eligibility_state,
  "blocking_reason", ARGV.block_blocking_reason,
  "blocking_detail", ARGV.block_blocking_detail,
  "terminal_outcome", "none",
  "attempt_state", core.attempt_state,  -- unchanged
  "public_state", ARGV.block_public_state,
  "last_transition_at", now_ms, "last_mutation_at", now_ms)

-- Move between sorted sets
redis.call("ZREM", KEYS[2], ARGV.execution_id)
redis.call("ZADD", KEYS[3], now_ms, ARGV.execution_id)

return ok("blocked")
```

**Caller invocation examples:**
- Budget denial: `block_eligibility_state = "blocked_by_budget"`, `block_blocking_reason = "waiting_for_budget"`, `block_blocking_detail = "budget budget-abc123: total_cost 48M/50M (96%)"`, `block_public_state = "rate_limited"`, `target_blocked_zset = ff:idx:{p:N}:lane:<lane>:blocked:budget`
- Quota denial: `"blocked_by_quota"`, `"waiting_for_quota"`, `"quota quota-xyz: tokens_per_minute 95K/100K, resets in 8s"`, `"rate_limited"`, `blocked:quota`
- Route denial: `"blocked_by_route"`, `"waiting_for_capable_worker"`, `"requires gpu=true, no registered worker has it"`, `"waiting"`, `blocked:route`
- Lane denial: `"blocked_by_lane_state"`, `"paused_by_policy"`, `"lane inference-fast is in draining state"`, `"waiting"`, `blocked:operator`

#### 12.10 Grant Expiry Reconciler

A background process periodically checks for executions that were removed from the eligible set (step 5 above) but never had their grant consumed:

1. Scan execution core records where `lp = runnable` AND `es = eligible_now` but execution is not in any eligible sorted set.
2. Check if a claim-grant key exists. If not (expired), re-add to the eligible set.
3. If a grant exists but is expired, delete it and re-add.

This prevents grant-related execution loss. The reconciler is a safety net — under normal operation, grants are consumed within milliseconds.

---

### 13. Error Model

| Error | When |
|-------|------|
| `lane_not_found` | Lane ID does not exist. |
| `lane_intake_closed` | Submission attempted on a paused/draining/disabled lane. |
| `lane_disabled` | Claim attempted on a disabled lane. |
| `no_eligible_execution` | Worker requested work but nothing matches. |
| `capability_mismatch` | Worker's capabilities do not satisfy execution requirements. |
| `no_capable_worker` | No registered worker can satisfy execution requirements. |
| `no_capacity_available` | Capable workers exist but all at max concurrent claims. |
| `budget_admission_denied` | Budget check prevents claim. |
| `quota_admission_denied` | Quota/rate-limit check prevents claim. |
| `execution_not_eligible` | Execution is not runnable, not eligible, not unowned, or not non-terminal. Returned by `issue_claim_grant`. |
| `execution_not_in_eligible_set` | Execution was removed from eligible set by another scheduler (race). Returned by `issue_claim_grant`. |
| `execution_not_reclaimable` | Execution is not in a reclaimable ownership state. Returned by `issue_reclaim_grant`. |
| `grant_already_exists` | Claim-grant already issued for this execution. |
| `grant_expired` | Claim-grant TTL elapsed before `acquire_lease` consumed it. |
| `grant_mismatch` | Worker/capability in `acquire_lease` doesn't match grant. |
| `worker_not_found` | Worker instance ID not registered. |
| `worker_not_active` | Worker exists but is draining/unhealthy/deregistered. |
| `invalid_lane_config` | Malformed lane configuration update. |
| `concurrency_limit_reached` | Lane's `max_concurrency` active executions already running. |

---

## Interactions with Other Primitives

| Primitive | RFC | Interaction |
|-----------|-----|-------------|
| **Execution** | RFC-001 | Scheduler reads `eligibility_state`, `priority`, `lane_id`, `routing_requirements`. Admission rejections update `eligibility_state` and `blocking_reason` on the execution. |
| **Attempt** | RFC-002 | Claim-grant consumption creates a new attempt (or continues a suspended one). Route snapshot is stored on `AttemptPolicySnapshot`. |
| **Lease** | RFC-003 | `ff_claim_execution` consumes the claim-grant atomically. Grant fields are validated inside the function. |
| **Suspension** | RFC-004 | Resumed executions re-enter the eligible sorted set and are subject to normal scheduling. |
| **Signal** | RFC-005 | Signals that trigger resume place the execution back in the eligible set for scheduling. |
| **Flow** | RFC-007 | Flow dependency satisfaction moves executions from `blocked_by_dependencies` to `eligible_now`, making them visible to the scheduler. |
| **Budget/Quota** | RFC-008 | Scheduler checks budget/quota admission before issuing claim-grants. Budget exhaustion moves executions to `blocked_by_budget`/`blocked_by_quota`. |

## Implementation Mapping

| Component | Crate | Notes |
|---|---|---|
| Claim cycle (§3.1 steps 1-5) | `ff-scheduler` | Owns the full claim sequence. Calls `ff-script` wrappers directly. Does NOT depend on `ff-engine`. |
| `ff_issue_claim_grant`, `ff_issue_reclaim_grant` | `ff-script` (wrappers), called by `ff-scheduler` | Single-partition FCALL. |
| `ff_block_execution_for_admission` | `ff-script` (wrapper), called by `ff-scheduler` (per-candidate budget/route/quota denial) | Single-partition FCALL. |
| `ff_unblock_execution` | `ff-script` (wrapper), called by `ff-engine::scanner` (eligibility re-evaluation) | Single-partition FCALL. Scanner detects cleared conditions. |
| Lane management (pause, drain, enable) | `ff-server` (operator API) | Direct Valkey writes to `ff:lane:<id>:config`. |
| Worker registration, heartbeat | `ff-sdk` (via `ff-script`) | Direct HSET/HEXPIRE to `ff:worker:<instance_id>`. |
| Fairness state (deficit counters) | `ff-scheduler` (in-memory + ephemeral Valkey keys) | Rebuilt from queue depths on restart. |
| Capability matching | `ff-scheduler` (in-memory) | Reads worker registrations, caches capability indexes. |
| `ff_check_admission_and_record` | `ff-script` (wrapper), called by `ff-scheduler` (step 4a of claim flow) | Single-partition FCALL on `{q:K}`. |
| `ff_promote_delayed` | `ff-script` (wrapper), called by `ff-engine::scanner` (delayed promoter) | Single-partition FCALL. |
| `explain_capability_mismatch` | `ff-server` (read-only API) | Direct Valkey reads: worker registrations + execution routing_requirements. |

---

## V1 Scope

### In V1

- Claim-grant model (scheduler computes, Lua consumes)
- Per-lane priority ordering with composite score
- Cross-lane weighted round-robin fairness
- Cross-tenant basic fair-share within lanes
- Capability matching: required/preferred/forbidden sets
- Locality hints (preferred and required modes)
- Worker registration with heartbeat TTL
- Lane object with full config, 4 states (`intake_open`, `intake_paused`, `draining`, `disabled`)
- All submission modes: fire-and-forget, delayed, scheduled, request/reply, bulk, deduplicated
- All lane control operations (pause, resume, drain, disable, enable)
- Explainable admission rejections mapped to RFC-001 blocking reasons
- Partition-local eligible sorted sets with priority scoring
- Claim-grant Valkey Function with double-grant prevention
- Grant expiry reconciler
- Worker capability index for fast lookup
- Lane counts (cached, periodically aggregated)

### Designed-for but deferred

- Weighted preference scoring beyond simple match/no-match
- Intra-lane aging to prevent low-priority starvation
- Global fairness scheduler (v1 uses per-scheduler-instance deficit tracking)
- Worker pool abstraction as a first-class object (v1 uses capability filtering)
- Route scoring with multiple candidate ranking
- Preemptive rerouting of waiting executions on worker availability changes
- Dynamic lane concurrency scaling
- Cross-region routing optimization

---

## Open Questions

1. ~~**Scheduler instance coordination:**~~ **Resolved.** The `ff_issue_claim_grant` (Valkey Function) script atomically ZREMs the execution from the eligible set (step 5) before writing the grant. If two schedulers race on the same candidate, only one wins the ZREM — the second finds the execution absent from the set and returns `execution_not_in_eligible_set`. No leader election required for v1. At very high scale, partition-affine scheduling (each scheduler owns a subset of partitions) can reduce wasted work. See RFC-010 §4.6.
2. ~~Request/reply implementation~~ — Resolved in §8.5: XREAD BLOCK on lease_history stream. No pub/sub needed.
3. **Flow-aware priority boosting:** Should the scheduler inherit or boost priority from the parent flow or coordinator execution? This would require cross-partition reads to the flow's `{fp:N}` partition. V1 ignores flow priority (§5.4). When should this be introduced — when DAG critical-path scheduling becomes a product requirement?

**Resolved:** Fairness window duration = 10 seconds default, configurable per deployment. Short enough to rebalance within seconds, long enough to avoid noisy oscillation.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitives 8 (Capability), 9 (Route), 10 (Submission/Queue facade), Section 4.4, Finalization §12
- RFC-001: Execution Object and State Model (worker-2)
- RFC-002: Attempt Model (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
- RFC-007: Flow and Dependency Model (worker-1)
- RFC-008: Budget, Quota, and Rate-Limit Policy (worker-3)
