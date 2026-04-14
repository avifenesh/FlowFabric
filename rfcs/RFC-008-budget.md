# RFC-008: Budget, Quota, and Rate-Limit Policy

**Status:** Draft
**Author:** FlowFabric Team (Worker-3)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 7, Primitive 7.5, Finalization item 11

---

## Summary

This RFC defines three **separate user-facing resource-control policy families**: budget (consumable allowance), quota (admission caps per window), and rate-limit (pacing and throughput shaping). They may share enforcement machinery internally but remain distinct in the public model because they govern different resources, trigger different breach behaviors, and require different configuration surfaces. This RFC covers the budget object, extensible dimensions, attachment model, usage reporting, enforcement timing and actions, the quota/rate-limit policy object, concurrency caps, and the Valkey data model with atomic usage-increment + breach-check Lua scripts.

## Motivation

FlowFabric runs AI inference, agent loops, and workflow executions that consume real money (tokens, API calls, compute). Without resource controls:

- **UC-37 (Token / cost accounting):** No way to cap a runaway agent loop that burns through $500 of tokens.
- **UC-39 (Flow-level budget enforcement):** A whole execution tree shares a budget, but without enforcement, any child can blow it.
- **UC-40 (Token-aware rate limiting):** A single tenant can starve others by flooding the system with high-token-count requests.
- **UC-48 (Provider quota / tenant quota enforcement):** No per-tenant fairness or provider-side rate respect.
- **UC-24 (Pause on policy or budget breach):** Execution must suspend or fail when a boundary is crossed, not silently continue.
- **UC-54 (Fair scheduling across groups):** Without quota/rate-limit, one hot tenant dominates the fleet.

Budget answers: **how much total allowance may this scope consume?**
Quota/rate-limit answers: **how fast or how often may this scope proceed?**

These are different questions with different breach semantics. Collapsing them into one API would force users to configure stop-the-world budget exhaustion and defer-to-next-window rate limits through the same interface.

---

## Detailed Design

### Part 1: Budget

#### 1.1 Budget Object Definition

A budget is a durable, inspectable, enforceable consumable allowance attached to one or more execution scopes.

##### Identity and Scope

| Field | Type | Required | Description |
|---|---|---|---|
| `budget_id` | `UUID` | Yes | Unique budget identity. |
| `scope_type` | `ScopeType` enum | Yes | `execution`, `flow`, `lane`, `tenant`. |
| `scope_id` | `String` | Yes | The ID of the scoped entity (execution_id, flow_id, lane_id, or tenant/namespace). |
| `name` | `String` | No | Human-readable name (e.g., "Q2 inference budget", "agent-loop-guardrail"). |
| `created_at` | `i64` (ms epoch) | Yes | Budget creation time. |
| `created_by` | `String` | Yes | Creator identity (operator, system, API caller). |

##### Limit Definitions

| Field | Type | Required | Description |
|---|---|---|---|
| `hard_limits` | `Map<String, u64>` | Yes | Dimension → hard cap. Breach triggers `on_hard_limit` action. Key is dimension name (extensible). Value is limit in the dimension's unit. |
| `soft_limits` | `Map<String, u64>` | No | Dimension → soft cap. Breach triggers `on_soft_limit` action (warning, degrade, etc.). Must be <= hard_limit for each dimension. |
| `currency` | `String` | No | Currency code for cost dimensions (e.g., `USD`). Default: `USD`. |
| `cost_unit` | `String` | No | Unit for cost values. Default: `microdollars` (1 USD = 1,000,000). |
| `reset_policy` | `ResetPolicy` | No | If set, budget resets periodically. `None` = lifetime budget. |

The `ResetPolicy`:

| Field | Type | Description |
|---|---|---|
| `reset_interval` | `String` | `hourly`, `daily`, `weekly`, `monthly`, `custom_seconds`. |
| `reset_seconds` | `u64` | Custom interval in seconds (used when `reset_interval = custom_seconds`). |
| `next_reset_at` | `i64` (ms epoch) | Next scheduled reset time. |
| `carry_over` | `bool` | Whether unused allowance carries over. Default: `false`. |

##### Usage State

| Field | Type | Required | Description |
|---|---|---|---|
| `current_usage` | `Map<String, u64>` | Yes | Dimension → current consumption. Incremented atomically via `report_usage`. |
| `last_updated_at` | `i64` (ms epoch) | Yes | Last usage increment time. |
| `breach_count` | `u32` | Yes | Total number of hard-limit breaches since creation or last reset. |
| `soft_breach_count` | `u32` | No | Total soft-limit breaches. |
| `last_breach_at` | `i64` (ms epoch) | No | Most recent breach time. |
| `last_breach_dimension` | `String` | No | Which dimension breached most recently. |
| `last_reset_at` | `i64` (ms epoch) | No | When the budget was last reset (for recurring budgets). Set atomically by `reset_budget`. `None` for lifetime budgets that have never been reset. |

##### Enforcement Policy

| Field | Type | Required | Description |
|---|---|---|---|
| `on_hard_limit` | `EnforcementAction` enum | Yes | What happens when a hard limit is breached. |
| `on_soft_limit` | `EnforcementAction` enum | No | What happens when a soft limit is breached. Default: `warn`. |
| `enforcement_mode` | `EnforcementMode` enum | Yes | `strict` (reject/act immediately), `advisory` (log only, no blocking). **Consumed by the engine's enforcement dispatch layer (outside Lua), not by the Lua script.** `report_usage_and_check.lua` always returns breach status; the enforcement dispatcher reads `enforcement_mode` to decide whether to call `fail_execution` (strict) or just log (advisory). |
| `escalation_target` | `String` | No | Operator or system to notify on breach (e.g., PagerDuty route, operator group). **Consumed by the notification layer** — v1 logs a warning with the target identifier, future versions may integrate with PagerDuty/Slack/webhooks. |

#### 1.2 Budget Dimensions

Budget dimensions are **extensible**, not a closed enum. The engine ships with well-known built-in dimensions and accepts arbitrary custom dimensions.

##### Built-in Dimensions (v1)

| Dimension | Unit | Description |
|---|---|---|
| `total_input_tokens` | tokens | Input/prompt tokens across all attempts. |
| `total_output_tokens` | tokens | Output/completion tokens across all attempts. |
| `total_tokens` | tokens | Sum of input + output + thinking tokens. |
| `total_thinking_tokens` | tokens | Thinking/reasoning tokens (model-dependent). |
| `total_cost` | microdollars | Total cost in microdollars (1 USD = 1,000,000). |

##### Future/Extensible Dimensions

| Dimension | Unit | Description |
|---|---|---|
| `latency_ms` | milliseconds | Cumulative or max wall-clock processing time. |
| `effort_units` | units | Provider-specific reasoning effort units. |
| `provider_reasoning_units` | units | Provider-specific reasoning token equivalent. |
| `custom:<name>` | varies | Arbitrary application-defined dimension. |

Custom dimensions use the `custom:` prefix by convention. The engine stores and enforces them identically to built-ins — the only difference is that built-ins have default UX treatment.

#### 1.3 Invariants

##### Invariant B1 — Explicit scope

Every budget has an explicit `scope_type` and `scope_id`. There are no implicit or ambient budgets. An execution is subject to a budget only if the budget is explicitly attached.

**Rationale:** Implicit scope leads to "why is this blocked?" confusion. Explicit attachment means the operator can always enumerate which budgets apply.

##### Invariant B2 — Usage and enforcement are coupled

If the engine claims to enforce a budget (enforcement_mode = strict), the usage increment and limit check must be atomic enough to avoid obvious overshoot races. A worker must not report 10K tokens and then have a second worker report another 10K before the first breach check runs.

**Rationale:** Non-atomic check-then-increment allows two concurrent workers to each pass a 50% budget check and collectively exceed 100%. The Lua script must HINCRBY + compare in one atomic operation.

##### Invariant B3 — Breach policy is explicit

A limit breach is never ambiguous. Each budget defines exactly what happens on hard-limit breach and optionally on soft-limit breach. The engine never silently ignores a breach in strict mode.

**Rationale:** Silent budget overruns are the #1 complaint in cost-governed AI systems. FlowFabric must make breach behavior deterministic and visible.

##### Invariant B4 — Inspectability

At any moment, operators can inspect: limit values, current consumption, remaining allowance, recent breaches, and which scope the budget is attached to. The response to "why is this blocked by budget?" must be specific (which dimension, how much consumed, what the limit is).

**Rationale:** "Blocked by budget" without details is useless for operators.

#### 1.4 Enforcement Actions

| Action | Applies To | Description |
|---|---|---|
| `suspend` | Active execution | Execution suspends (RFC-004) with `reason_code = paused_by_budget`. Resumes when budget is increased or reset. |
| `fail` | Active execution | Execution fails with `failure_reason = budget_exceeded`. Attempt ends. |
| `deny_child` | Flow child spawn | New child execution creation is rejected. Parent continues. |
| `warn` | Any | Log warning, emit metric, notify escalation target. Execution continues. |
| `reroute_fallback` | Active execution | Advance to next fallback tier (cheaper provider/model). Creates new attempt (RFC-002). |
| `close_scope` | Lane or tenant | Stop all new work in the affected scope. Existing active work may continue or be suspended per policy. |

`on_hard_limit` default: `fail`.
`on_soft_limit` default: `warn`.

#### 1.5 Budget Attachment Model

A budget is attached to exactly one scope. Multiple budgets may apply to one execution simultaneously through scope hierarchy.

| Scope | Attachment Semantics | Example |
|---|---|---|
| `execution` | One budget governs one execution. | Max $5 per single LLM call. |
| `flow` | One budget governs all executions in a flow. Usage aggregated across members. | Max 500K tokens for entire agent loop. |
| `lane` | One budget governs all executions submitted to a lane. | Max $1000/day for the inference-fast lane. |
| `tenant` | One budget governs all executions for a tenant/namespace. | Monthly $10K cap for workspace X. |

**Simultaneous budgets:** When an execution has multiple budgets (e.g., its own execution-scoped budget + the flow-scoped budget + the tenant-scoped budget), a usage report is checked against ALL attached budgets. The **most restrictive** breach triggers the enforcement action. The engine records which specific budget caused the block.

**Budget resolution order for an execution:**
1. Execution-scoped budget (if attached)
2. Flow-scoped budget (if execution is in a flow with a budget)
3. Lane-scoped budget (from the execution's lane)
4. Tenant-scoped budget (from the execution's namespace)

All are checked. Any breach blocks.

#### 1.6 Usage Reporting

Usage is reported through explicit increment operations. The engine does NOT infer usage — workers and the control plane report it.

**Post-hoc charge model (v1):** Workers report actual usage after it is known. This is the primary model because LLM token counts are only known after inference completes (or incrementally during streaming).

**Reporting flow:**
1. Worker streams tokens → appends `usage_update` frame to attempt stream (RFC-006).
2. Worker reports usage delta → calls `report_usage(execution_id, attempt_index, lease_epoch, usage_delta)`.
3. Engine step (a): Lua on `{p:N}` — validates lease, HINCRBY attempt usage hash (all dimensions), HINCRBY execution core usage (built-in dimensions only). Returns attached `budget_ids` (read from exec core `budget_ids` field).
4. Engine step (b): For each attached budget, calls `report_usage_and_check.lua` on the budget's `{b:M}` partition (separate Lua call, NOT atomic with step 3a). Returns breach status per budget.
5. If breach: engine applies enforcement action. Worker receives the breach in the response.

**Usage delta structure:**

| Field | Type | Description |
|---|---|---|
| `dimensions` | `Map<String, u64>` | Dimension → increment value. Only non-zero dimensions need to be included. |

**Three-counter consistency model:** Each `report_usage` call touches three usage counters:

| Counter | Location | Updated How | Authoritative For |
|---|---|---|---|
| Attempt usage | `ff:attempt:{p:N}:{eid}:{idx}:usage` | HINCRBY in Lua on `{p:N}` — ALL dimensions including custom | Per-attempt attribution (which run consumed what). |
| Execution usage | `ff:exec:{p:N}:{eid}:core` (total_input_tokens, etc.) | HINCRBY in same Lua script on `{p:N}` — **built-in dimensions only** | Execution-level inspection (fast read, no scan of attempt hashes). |
| Budget usage | `ff:budget:{b:M}:{bid}:usage` | HINCRBY in separate Lua on `{b:M}` — ALL dimensions including custom | Budget enforcement (breach checks). |

**Built-in dimensions for exec core HINCRBY** (hardcoded in the Lua script): `total_input_tokens`, `total_output_tokens`, `total_thinking_tokens`, `total_cost_micros`, `total_latency_ms`. Custom dimensions (e.g., `custom:effort_units`) are tracked on the attempt usage hash and budget usage hash but NOT on the execution core hash. This prevents unbounded custom fields from polluting the core hash.

Attempt and execution counters are updated atomically in one Lua script (same `{p:N}` partition). Budget counters are updated in a separate call to `{b:M}`. At any point in time, attempt and execution counters agree with each other for built-in dimensions, but budget counters may lag by one concurrent `report_usage` call. This is the documented cross-partition consistency model from RFC-010 §3.3.

#### 1.7 Enforcement Timing

Budget checks happen at several distinct points in the execution lifecycle:

| Enforcement Point | When | What is Checked |
|---|---|---|
| Before claim | Scheduler evaluates budget before allowing a worker to claim. | All attached budgets for the execution. |
| During active execution | On each `report_usage` call. | Incremental usage against remaining budget. |
| Before child spawn | When a flow execution creates a child. | Flow-scoped budget remaining allowance. |
| Before fallback escalation | When advancing to next fallback tier. | Budget for the new tier's expected cost. |
| Before resume from suspension | When a suspended execution becomes eligible. | All attached budgets (may have changed during suspension). |

**Enforcement strictness depends on scope colocation:**

For **execution-scoped** budgets (colocated on the same `{p:N}` partition as the execution):
- All enforcement points are **strict** (Class A atomic). The budget usage hash shares the partition with the execution core, so HINCRBY + limit check + execution state update can run in one Lua script.

For **cross-partition** budgets (flow-scoped on `{b:M}`, lane-scoped, tenant-scoped):
- Enforcement is **best-effort-consistent**, not atomic with claim or spawn. The caller increments the budget on the budget's `{b:M}` partition in a separate Lua call. Between the budget check and the execution state transition, another concurrent request may also pass. Overshoot by one concurrent request is accepted.
- The budget is rechecked at the next enforcement point (e.g., during `report_usage` if the budget was checked at claim time). This provides convergent correctness without requiring cross-partition atomicity.
- **Before claim**: Scheduler checks budget (advisory read), issues claim grant. Grant consumption does not re-validate cross-partition budget.
- **During active**: `report_usage` increments the cross-partition budget and checks limits. If breached, the worker receives the breach in the response and the engine applies the enforcement action.
- **Before child spawn**: Caller checks flow-scoped budget before `add_execution_to_flow`. If exhausted with `on_hard_limit = deny_child`, the spawn is rejected. This check is a separate Lua call to the budget partition — not atomic with the flow mutation.
- **Before resume**: Advisory check. Re-evaluated at claim time.

This design avoids impossible cross-slot atomicity in Valkey Cluster while providing convergent enforcement. The worst case is one extra request slipping through before the breach is detected at the next enforcement point.

---

### Part 2: Quota and Rate-Limit Policy

#### 2.1 Quota Policy Object Definition

A quota policy governs **admission and pacing** — how fast or how often a scope may proceed.

| Field | Type | Required | Description |
|---|---|---|---|
| `quota_policy_id` | `UUID` | Yes | Unique policy identity. |
| `scope_type` | `ScopeType` enum | Yes | `lane`, `tenant`, `provider`, `model_tier`. |
| `scope_id` | `String` | Yes | Scoped entity ID. |
| `name` | `String` | No | Human-readable name. |
| `created_at` | `i64` (ms epoch) | Yes | Creation time. |
| `created_by` | `String` | Yes | Creator identity. |

##### Rate Limits

| Field | Type | Required | Description |
|---|---|---|---|
| `requests_per_window` | `RateLimit` | No | Max requests per time window. |
| `tokens_per_minute` | `RateLimit` | No | Max tokens per minute (input + output). |
| `cost_per_minute` | `RateLimit` | No | Max cost (microdollars) per minute. |
| `custom_rate_limits` | `Map<String, RateLimit>` | No | Extensible named rate limits. |

The `RateLimit` structure:

| Field | Type | Description |
|---|---|---|
| `limit` | `u64` | Maximum value per window. |
| `window_seconds` | `u64` | Window duration in seconds. |
| `window_type` | `WindowType` | `sliding` or `fixed`. |

All time-based comparisons in sliding window scripts use Valkey server time (`redis.call("TIME")`). Clock accuracy depends on the Valkey host's NTP configuration. For windows shorter than 10 seconds, clock skew between Valkey Cluster nodes may cause ~1% capacity variance.

##### Concurrency Caps

| Field | Type | Required | Description |
|---|---|---|---|
| `active_concurrency_cap` | `u32` | No | Max simultaneously active (leased) executions for this scope. |
| `per_tenant_concurrency_cap` | `u32` | No | Max active executions per tenant within this scope. |
| `per_lane_concurrency_cap` | `u32` | No | Max active executions per lane within this scope. |

##### Enforcement Policy

| Field | Type | Required | Description |
|---|---|---|---|
| `on_rate_breach` | `QuotaAction` enum | Yes | What happens when a rate limit is exceeded. |
| `on_concurrency_breach` | `QuotaAction` enum | Yes | What happens when a concurrency cap is hit. |
| `jitter_ms` | `u64` | No | Random jitter added to delay on breach. Default: `0`. |

#### 2.2 Quota Invariants

##### Invariant QL1 — Distinct public policy surface

Users configure quota/rate-limit policy separately from budget policy. They are different API objects with different fields, different operations, and different dashboards.

**Rationale:** Conflating "you've spent too much" (budget) with "you're going too fast" (rate limit) leads to confusing configuration and surprising breach behavior.

##### Invariant QL2 — Distinct breach behavior

Quota breaches trigger **deferral**, not termination. Rate-limit breaches may jitter, back off, or throttle. Budget breaches may suspend or fail. These are fundamentally different responses.

**Rationale:** A rate-limit breach that fails an execution is almost always wrong. A budget breach that merely delays is almost always wrong. Separate policy families mean separate defaults.

##### Invariant QL3 — Explainable admission failure

If an execution cannot proceed because of quota or rate limit, the engine exposes: which policy, which dimension, current window usage, and when the window resets.

**Rationale:** "Rate limited" without details is useless. Operators need to know "tokens_per_minute: 95K/100K, resets in 12s."

#### 2.3 Quota Enforcement Actions

| Action | Description |
|---|---|
| `deny_and_retry_later` | Reject claim now. Execution stays in `waiting`. Worker retries on next poll. |
| `delay_until_window` | Move execution to `delayed` with `eligible_after` = next window boundary. |
| `delay_with_jitter` | Same as above but add random jitter to spread thundering herd. |
| `throttle` | Reduce throughput for the scope (implementation: increase delay between claims). |
| `warn` | Log, emit metric. Execution proceeds. |

`on_rate_breach` default: `delay_until_window`.
`on_concurrency_breach` default: `deny_and_retry_later`.

#### 2.4 Blocking Reasons

Budget and quota blocks map to RFC-001's `blocking_reason` enum. Fine-grained causes go in the `blocking_detail` string (RFC-001 §5), not the enum.

| `blocking_reason` (enum) | Source | `blocking_detail` (string, examples) | Public State |
|---|---|---|---|
| `waiting_for_budget` | Budget hard limit reached, execution blocked. | `"budget budget-abc123: total_cost 48M/50M (96%)"` | `rate_limited` |
| `waiting_for_quota` | Rate-limit window full. | `"quota quota-xyz: requests_per_window 100/100, resets in 12s"` | `rate_limited` |
| `waiting_for_quota` | Token rate limit hit. | `"quota quota-xyz: tokens_per_minute 95K/100K, resets in 8s"` | `rate_limited` |
| `waiting_for_quota` | Concurrency cap reached. | `"quota quota-xyz: active_concurrency 50/50"` | `rate_limited` |

**V1 soft limits:** `on_soft_limit` defaults to `warn`. The Lua script returns `SOFT_BREACH` to the caller, which logs a warning and emits a metric. No execution state change occurs — the execution continues running. No `blocking_reason` is set. Graduated soft-limit enforcement (block, suspend, reroute on soft breach) is deferred to post-v1.

When budget enforcement action is `suspend` (post-v1), the execution transitions to `suspended` with `blocking_reason = paused_by_budget` (an existing RFC-001 blocking_reason value used when lifecycle_phase = suspended).

When budget enforcement action is `fail`, the execution transitions to `failed` with `failure_reason = budget_exceeded`. This is a terminal outcome, not a blocking reason.

The `blocking_detail` string is populated by the engine at the enforcement point and is available via `get_execution_state` and `get_blocking_reason` APIs (RFC-001). It provides the dimension, current value, limit, and reset timing that operators need without expanding the blocking_reason enum.

---

### Part 3: Operations

#### 3.1 Budget Operations

| Operation | Class | Parameters | Semantics |
|---|---|---|---|
| `create_budget` | A | `budget_spec` | Creates a new budget object. Does not attach it. |
| `attach_budget` | A | `budget_id`, `scope_type`, `scope_id` | Attaches a budget to a scope. Validates no conflicting attachment for the same scope+dimension. |
| `detach_budget` | A | `budget_id`, `scope_type`, `scope_id` | Removes attachment. Active executions under this budget are re-evaluated. |
| `get_budget` | C | `budget_id` | Returns full budget record with current usage. |
| `get_budget_usage` | C | `budget_id` | Returns usage map, remaining allowance per dimension, and breach status. |
| `report_usage` | A | `execution_id`, `attempt_index`, `lease_epoch`, `usage_delta` | Atomically increments usage on all attached budgets. Returns breach status per budget. |
| `check_budget` | C | `execution_id` | Reads all attached budgets and returns whether the execution can proceed. Does not mutate. |
| `override_budget` | A | `budget_id`, `operator_action` (increase_limit, reset_usage, change_enforcement) | Operator override with audit. |
| `reset_budget` | A | `budget_id` | Resets `current_usage` to zero. Records reset event. |

#### 3.2 Quota/Rate-Limit Operations

| Operation | Class | Parameters | Semantics |
|---|---|---|---|
| `create_quota_policy` | A | `quota_policy_spec` | Creates a new quota/rate-limit policy. |
| `attach_quota_policy` | A | `quota_policy_id`, `scope_type`, `scope_id` | Attaches policy to a scope. |
| `check_admission` | A | `execution_id`, `scope_ids` | Checks all quota/rate-limit policies for the execution's scopes. Returns admit/deny + reason. |
| `record_admission` | A | `execution_id`, `scope_ids`, `usage_delta` | Records that an execution was admitted. Increments window counters. |
| `get_quota_policy` | C | `quota_policy_id` | Returns the policy definition. |
| `get_quota_status` | C | `scope_type`, `scope_id` | Returns current window usage, remaining capacity, next reset time, active concurrency count. |
| `override_quota` | A | `quota_policy_id`, `operator_action` | Operator override with audit. |

---

### Part 4: Valkey Data Model

#### 4.1 Partitioning

Budget and quota keys use `{p:N}` partition tags where possible. However, budgets may span multiple partitions (a tenant-scoped budget governs executions across many partitions). Cross-partition budget enforcement uses a **budget-local partition** — the budget itself lives on one partition determined by `budget_id`, and usage increments are routed to that partition.

```
Budget partition: {b:M} where M = crc16(budget_id) % partition_count
```

For execution-scoped budgets that share a partition with the execution, the budget key uses the execution's `{p:N}` tag for colocated atomic scripts.

#### 4.2 Budget Key Schema

**Budget definition (hash):**
```
ff:budget:{b:M}:{budget_id}  →  HASH
  budget_id         → UUID
  scope_type        → tenant
  scope_id          → workspace-abc
  name              → Q2 inference budget
  created_at        → 1713100800000
  created_by        → operator-jane
  currency          → USD
  cost_unit         → microdollars
  enforcement_mode  → strict
  on_hard_limit     → fail
  on_soft_limit     → warn
  escalation_target → pagerduty:budget-alerts
  breach_count      → 2
  soft_breach_count → 5
  last_breach_at    → 1713200000000
  last_breach_dim   → total_cost
  last_updated_at   → 1713200100000
  last_reset_at     → 1712016000000
```

**Budget limits (hash, separate for clean reads):**
```
ff:budget:{b:M}:{budget_id}:limits  →  HASH
  hard:total_tokens         → 1000000
  hard:total_cost           → 50000000
  hard:total_thinking_tokens→ 200000
  soft:total_tokens         → 800000
  soft:total_cost           → 40000000
```

**Budget usage (hash, hot path — HINCRBY target):**
```
ff:budget:{b:M}:{budget_id}:usage  →  HASH
  total_input_tokens    → 245000
  total_output_tokens   → 89000
  total_tokens          → 350000
  total_thinking_tokens → 16000
  total_cost            → 12500000
```

Separating usage from limits and definition avoids loading the full budget record on every increment. The hot path touches only the usage hash.

**Budget attachment index (set per scope):**
```
ff:budget_attach:{b:M}:{scope_type}:{scope_id}  →  SET
  member: budget_id
```

**Reverse index — executions attached to a budget:**
```
ff:budget:{b:M}:{budget_id}:executions  →  SET
  member: execution_id
```

**Budget reset schedule (sorted set, global):**
```
ff:idx:{b:M}:budget_resets  →  ZSET
  member: budget_id
  score:  next_reset_at (ms)
```

#### 4.3 Quota Key Schema

**Quota policy definition (hash):**
```
ff:quota:{q:K}:{quota_policy_id}  →  HASH
  quota_policy_id     → UUID
  scope_type          → lane
  scope_id            → inference-fast
  name                → inference rate limit
  created_at          → 1713100800000
  on_rate_breach      → delay_until_window
  on_concurrency_breach → deny_and_retry_later
  jitter_ms           → 500
```

**Rate-limit windows (sorted set — sliding window pattern):**
```
ff:quota:{q:K}:{quota_policy_id}:window:{dimension}  →  ZSET
  member: {request_id or execution_id}:{timestamp_ms}
  score:  timestamp_ms
```

Sliding window implementation: on each admission check, `ZREMRANGEBYSCORE` removes entries older than `now - window_seconds * 1000`, then `ZCARD` gives current count. If count < limit, admit and `ZADD`.

**Concurrency counter (string, atomic INCR/DECR):**
```
ff:quota:{q:K}:{quota_policy_id}:concurrency  →  STRING
  value: current active count
```

Incremented on lease acquire, decremented on lease release/expire. Must be decremented in the same Lua scripts that handle lease release (RFC-003).

**Quota attachment index:**
```
ff:quota_attach:{q:K}:{scope_type}:{scope_id}  →  SET
  member: quota_policy_id
```

#### 4.4 Atomic Usage Report + Breach Check (Lua Script)

`report_usage_and_check.lua` — **check-before-increment**: reads current usage, checks limits, only HINRBYs if under limit. Since this runs as a Lua script on one `{b:M}` partition, the read-check-write sequence is **atomic** — concurrent calls are serialized by Valkey. This eliminates budget overshoot entirely (previously, HINCRBY-before-check allowed N concurrent workers to overshoot by N×delta).

```lua
-- KEYS (on budget partition {b:M}):
--   [1] budget_usage_hash
--   [2] budget_limits_hash
--   [3] budget_def_hash
-- ARGV:
--   [1] dimension_count (N)
--   [2..N+1] dimension names
--   [N+2..2N+1] dimension increments
--   [2N+2] now_ms

local dim_count = tonumber(ARGV[1])
local now_ms = ARGV[2 * dim_count + 2]
local breached_hard = nil
local breached_soft = nil

-- Phase 1: CHECK all dimensions BEFORE any increment.
-- If any hard limit would be breached, reject the entire report.
for i = 1, dim_count do
  local dim = ARGV[1 + i]
  local delta = tonumber(ARGV[1 + dim_count + i])
  local current = tonumber(redis.call("HGET", KEYS[1], dim) or "0")
  local new_total = current + delta

  -- Check hard limit (reject entire report if ANY dimension breaches)
  local hard_limit = redis.call("HGET", KEYS[2], "hard:" .. dim)
  if hard_limit and tonumber(hard_limit) > 0 and new_total > tonumber(hard_limit) then
    if not breached_hard then
      breached_hard = dim
      -- Record breach metadata but DO NOT increment
      redis.call("HINCRBY", KEYS[3], "breach_count", 1)
      redis.call("HSET", KEYS[3],
        "last_breach_at", now_ms,
        "last_breach_dim", dim,
        "last_updated_at", now_ms)
      local action = redis.call("HGET", KEYS[3], "on_hard_limit")
      return { "HARD_BREACH", dim, action, tostring(current), tostring(hard_limit) }
    end
  end
end

-- Phase 2: No hard breach detected — safe to increment all dimensions.
for i = 1, dim_count do
  local dim = ARGV[1 + i]
  local delta = tonumber(ARGV[1 + dim_count + i])
  local new_val = redis.call("HINCRBY", KEYS[1], dim, delta)

  -- Check soft limit (advisory — increment still happens)
  local soft_limit = redis.call("HGET", KEYS[2], "soft:" .. dim)
  if soft_limit and tonumber(soft_limit) > 0 and new_val > tonumber(soft_limit) then
    if not breached_soft then
      breached_soft = dim
    end
  end
end

-- Update metadata
redis.call("HSET", KEYS[3], "last_updated_at", now_ms)

if breached_soft then
  redis.call("HINCRBY", KEYS[3], "soft_breach_count", 1)
  local action = redis.call("HGET", KEYS[3], "on_soft_limit")
  return { "SOFT_BREACH", breached_soft, action }
end

return { "OK" }
```

**Zero-overshoot guarantee:** Because this is a Lua script, the read-check-write sequence is atomic on the `{b:M}` partition. Concurrent `report_usage` calls are serialized by Valkey. If two workers race and one would push the budget over the limit, the first to execute wins (increments), the second finds the budget already at or above the limit and gets HARD_BREACH (no increment). Budget usage never exceeds the hard limit by more than one Lua-script-width of concurrency — which is zero, because Lua scripts block the shard.

**Important:** For execution-scoped budgets colocated on `{p:N}`, this script can run within the same partition as the attempt usage update. For cross-partition budgets (flow, lane, tenant), the caller makes a separate Lua call to the budget's `{b:M}` partition. The attempt-level usage update (RFC-002) and the budget update are NOT in one atomic script for cross-partition budgets. However, the budget counter itself is now strictly accurate — the only inconsistency is that attempt-level usage may record tokens that the budget rejected. The budget reconciler (§6.5) handles this safely because it corrects upward only.

#### 4.5 Atomic Admission Check (Lua Script)

`check_admission_and_record.lua` — sliding-window rate check + concurrency check:

```lua
-- KEYS (on quota partition {q:K}):
--   [1] window_zset (for the specific rate dimension)
--   [2] concurrency_counter
--   [3] quota_def_hash
--   [4] admitted_guard_key (ff:quota:{q:K}:<policy_id>:admitted:<execution_id>)
-- ARGV:
--   [1] now_ms
--   [2] window_seconds
--   [3] rate_limit
--   [4] concurrency_cap
--   [5] execution_id
--   [6] jitter_ms

local now_ms = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2]) * 1000
local rate_limit = tonumber(ARGV[3])
local concurrency_cap = tonumber(ARGV[4])

-- Idempotency guard: if this execution was already admitted in this window,
-- return immediately. Prevents double-counting on retry (lost ACK scenario).
if redis.call("EXISTS", KEYS[4]) == 1 then
  return { "ALREADY_ADMITTED" }
end

-- Sliding window: remove expired entries
redis.call("ZREMRANGEBYSCORE", KEYS[1], "-inf", now_ms - window_ms)

-- Check rate — member is execution_id ALONE (not execution_id:timestamp).
-- ZADD on existing member updates score (no-op for count), making retries safe.
local current_count = redis.call("ZCARD", KEYS[1])
if rate_limit > 0 and current_count >= rate_limit then
  -- Find oldest entry to compute when window opens
  local oldest = redis.call("ZRANGE", KEYS[1], 0, 0, "WITHSCORES")
  local retry_after_ms = 0
  if #oldest >= 2 then
    retry_after_ms = tonumber(oldest[2]) + window_ms - now_ms
  end
  local jitter = math.random(0, tonumber(ARGV[6]))
  return { "RATE_EXCEEDED", retry_after_ms + jitter }
end

-- Check concurrency
if concurrency_cap > 0 then
  local active = tonumber(redis.call("GET", KEYS[2]) or "0")
  if active >= concurrency_cap then
    return { "CONCURRENCY_EXCEEDED" }
  end
end

-- Admit: record in window (execution_id as member — idempotent on retry)
redis.call("ZADD", KEYS[1], now_ms, ARGV[5])
-- Set admitted guard key with TTL = window size (prevents double-INCR on retry)
redis.call("SET", KEYS[4], "1", "PX", window_ms, "NX")
if concurrency_cap > 0 then
  -- Only INCR if this is the first admission (SET NX succeeded → key was new)
  -- NX flag on SET above returns nil if key existed, but we already returned
  -- ALREADY_ADMITTED above. If we reach here, the guard was just created.
  redis.call("INCR", KEYS[2])
end

return { "ADMITTED" }
```

**Concurrency decrement:** When a lease is released (RFC-003 complete/fail/suspend/expire), the concurrency counter must be decremented. This decrement is **asynchronous and cross-partition** — the lease-release Lua script (on `{p:N}`) cannot atomically DECR a counter on `{q:K}`. Instead, the control plane issues a separate DECR to the quota partition after the lease-release script completes.

Because the decrement is async, the counter may temporarily overcount (showing more active executions than actually exist). A **concurrency reconciler** runs periodically to correct drift: it counts actual active leases for the scope and resets the counter if it diverges. This is the same pattern as RFC-009's grant expiry reconciler — a safety net for cross-partition eventual consistency.

The reconciler also catches the case where a Valkey crash loses an INCR but not the subsequent lease (or vice versa). Reference RFC-003 for the lease-release scripts that should trigger the async DECR.

#### 4.6 Budget Reset Scanner

A background scanner processes `ff:idx:{b:M}:budget_resets`:

1. `ZRANGEBYSCORE ff:idx:{b:M}:budget_resets -inf <now> LIMIT 0 batch_size`
2. For each due budget:
   - Run `reset_budget.lua` on the budget's `{b:M}` partition.
   - Zero all usage counters.
   - Record reset event.
   - Compute and set `next_reset_at`.
   - Re-score in the reset index.

---

### Error Model

#### Budget Errors

| Error | When |
|---|---|
| `budget_not_found` | Requested budget_id does not exist. |
| `invalid_budget_scope` | Scope type or scope_id is malformed. |
| `budget_exceeded` | Hard limit breached during usage report. |
| `budget_soft_exceeded` | Soft limit breached (advisory). |
| `budget_attach_conflict` | Budget already attached to this scope, or conflicting budget exists. |
| `budget_override_not_allowed` | Override attempted without operator privileges. |

**Note on dimension extensibility:** `report_usage` accepts HINCRBY on any dimension name. Dimensions without a corresponding hard/soft limit in the limits hash are tracked (usage counter incremented) but never trigger a breach. This is the extensible design — new dimensions can be reported without reconfiguring the budget. Only dimensions with explicit limits are enforced. There is no `unsupported_usage_dimension` error in v1.

#### Quota/Rate-Limit Errors

| Error | When |
|---|---|
| `quota_policy_not_found` | Requested policy does not exist. |
| `rate_limit_exceeded` | Rate limit window is full. Includes `retry_after_ms`. |
| `concurrency_limit_exceeded` | Active concurrency cap reached. |
| `quota_attach_conflict` | Policy already attached to this scope. |
| `invalid_quota_spec` | Malformed policy definition. |

---

## Interactions with Other Primitives

### RFC-001 — Execution

- `eligibility_state = blocked_by_budget` when a budget check fails at claim time.
- `eligibility_state = blocked_by_quota` when a rate-limit or concurrency check fails.
- `blocking_reason` carries the cause (`waiting_for_budget` or `waiting_for_quota`). Fine-grained details (which dimension, current value, limit) go in `blocking_detail` string.
- `public_state = rate_limited` for both budget and quota blocks (budget blocks may also render as `suspended` if enforcement = suspend).
- ExecutionPolicySnapshot (RFC-001) carries `budget_ids: Vec<UUID>` listing attached budgets.

### RFC-002 — Attempt

- Per-attempt usage attribution (RFC-002 `UsageSummary`) is the source data for budget increments.
- `report_usage` takes `attempt_index` and `lease_epoch` to validate the caller.
- Budget breach during `report_usage` may trigger attempt termination (`ended_failure` with `failure_category = budget_exceeded`).

### RFC-007 — Flow (later batch, W1)

- Flow-scoped budgets aggregate usage across all member executions.
- `deny_child` enforcement prevents new child spawning when flow budget is exhausted.
- Flow budget attachment is recorded on the flow object.

### RFC-009 — Admission / Scheduling (later batch, W2)

- Quota `check_admission` is called by the scheduler before issuing claim grants.
- Concurrency counters are decremented by lease-release operations (RFC-003).
- Admission decisions are recorded in the execution's RuntimeAdmissionContext.

---

## V1 Scope

### V1 must-have

- Budget object with hard limits, extensible dimensions, enforcement policy.
- Built-in dimensions: total_input_tokens, total_output_tokens, total_tokens, total_thinking_tokens, total_cost.
- Enforcement actions: fail, warn. (Suspend and reroute_fallback are v1-should-have.)
- Budget attachment to execution, flow, lane, tenant scopes.
- Atomic usage report + breach check via Lua.
- Quota/rate-limit as separate policy object.
- Sliding-window rate limiting for requests_per_window and tokens_per_minute.
- Active concurrency cap.
- Admission check + record via Lua.
- Budget and quota inspection APIs.
- Operator override for budget limits and usage reset.
- Blocking reasons mapped to RFC-001 state vector.

### Designed-for but deferred

- Soft limits with graduated enforcement (v1 soft limits emit warnings only).
- Budget reservation / precharge semantics.
- Recurring budget reset (v1 supports it in schema; reset scanner is v1-should-have).
- `reroute_fallback` and `suspend` enforcement actions (require deeper integration with fallback and suspension paths).
- `close_scope` enforcement (requires lane-level enforcement).
- Per-provider quota policies.
- Cost injection from routing/fallback policy (v1 relies on worker-reported cost).
- Fixed-window rate limiting (v1 uses sliding window).
- Cross-partition budget enforcement with stronger consistency guarantees.
- Priority-aware budget reservation (see Known Limitations below).

### Known Limitations

**Shared budgets have no priority awareness.** When executions of different priorities share a budget (flow-scoped, lane-scoped, or tenant-scoped), the budget system has no concept of priority. Budget is consumed in execution order (first to report usage claims the headroom), not priority order. This creates a **budget-mediated priority inversion**:

1. High-priority execution is created and claimed (highest priority, budget has headroom).
2. High-priority execution suspends (e.g., waiting for human approval).
3. While suspended, many low-priority executions are claimed and consume the shared budget.
4. High-priority execution resumes, is re-claimed by a worker.
5. Worker reports usage → budget breached → enforcement action (fail) punishes the high-priority execution.

The high-priority execution was starved because low-priority work consumed the budget during its suspension. No mechanism exists to reserve budget headroom for high-priority work, preempt low-priority consumers, or order enforcement by priority.

**V1 mitigations (operator-controlled):**

- **(a) Separate budgets per priority tier:** Attach distinct budgets to high-priority and low-priority lanes or execution groups. Each tier has its own limit.
- **(b) Soft limits on shared budgets, hard limits on per-execution budgets:** Use shared budgets for monitoring only (`on_hard_limit = warn`). Apply hard limits at the execution level where priority inversion cannot occur.
- **(c) Per-execution budget guards:** Attach a small per-execution budget as a guardrail. The shared budget provides visibility; the per-execution budget provides protection.

**Designed-for extension:** Priority-aware budget reservation — the scheduler reserves a configurable fraction of shared budget headroom for executions above a priority threshold. Below the threshold, the budget appears exhausted even if headroom exists. This requires the scheduler to read priority + budget state together, which is a cross-partition operation. Deferred to post-v1.

---

## Open Questions

**Resolved:** Q1 — Quota scope hierarchy. Stack semantics for v1 — both apply, most restrictive breach triggers. Same semantics as budget attachment hierarchy (RFC-008 §1.5). Cascade overrides deferred to post-v1.

**Resolved:** Budget overshoot tolerance — one concurrent request overshoot is accepted for cross-partition budgets. Confirmed by RFC-010 §3.3 (cross-partition operation catalog) and §6.5 (budget reconciler). Budget is rechecked at every enforcement point.

**Resolved:** Concurrency counter consistency — the concurrency reconciler described in §4.5 handles drift from Valkey crashes. It periodically counts actual active leases and corrects the counter.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 7, Primitive 7.5, Finalization item 11
- RFC-001: Execution Object and State Model (worker-2)
- RFC-002: Attempt Model and Execution Lineage (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
- RFC-007: Flow Container and DAG Semantics (worker-1, later batch)
- RFC-009: Scheduling, Fairness, and Admission (worker-2, later batch)
