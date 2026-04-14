# RFC-007: Flow Container and DAG Semantics

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 6, Finalization items 9 and 10

---

## Summary

This RFC defines the Flow container: a real, durable coordination object with optional root execution, explicit membership, explicit dependency edges, policy inheritance, and derived flow-level state. The flow container is passive. It does not own leases, waitpoints, or suspension behavior itself; if orchestration must actively run or wait, that behavior lives on a real coordinator execution inside the flow. DAG is in scope for v1, with success-only dependencies and dependency-local failure propagation as the default semantics.

## Motivation

FlowFabric needs a first-class coordination container for:

- parent-child orchestration
- chain and stage pipelines
- fan-out / fan-in
- DAG execution
- dynamic child spawning
- flow-scoped policies and budgets
- flow-level inspection and cancellation

Without a real flow object, teams are forced to infer topology from scattered execution metadata. That breaks inspectability, cycle validation, policy inheritance, and any sensible explanation of "why is this node blocked?" or "why is this flow still open?"

## Detailed Design

### Object Definition

A flow is a durable coordination container that groups executions and edges under one coordination scope.

Locked design decisions:

- a flow is real even without a root execution
- a root execution is common and useful, but optional
- the flow container is passive
- if a flow needs to wait, suspend, signal, or resume, a coordinator execution inside the flow does that work

#### Flow object

##### Identity

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `flow_id` | UUID | yes | Stable identity of the coordination container. |
| `flow_partition_id` | u32 | yes | Valkey flow partition: `hash(flow_id) % num_partitions`, computed once and immutable. |
| `flow_name` | string | no | Human-facing name. |
| `flow_kind` | string | yes | Product-level flow category, e.g. `agent_run`, `approval_pipeline`, `etl_graph`. |
| `created_at` | unix ms | yes | Creation time. |
| `created_by` | string | yes | Actor that created the flow. |
| `namespace` | string | yes | Tenant or workspace scope. |

##### Structure

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `root_execution_id` | nullable UUID | no | Optional root/coordinator execution. |
| `has_root_execution` | bool | yes | Explicitly distinguishes rootless flows from flows whose root is still unresolved. |
| `topology_kind` | enum | yes | `tree`, `chain`, `fan_out_fan_in`, `dag`. |
| `node_count` | u32 | yes | Total member executions. |
| `edge_count` | u32 | yes | Total dependency edges. |
| `dynamic_expansion_enabled` | bool | yes | Whether executions and edges may be added after creation. |
| `graph_revision` | u64 | yes | Monotonic structural revision for membership and edge mutation grants. |

##### Policy

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `failure_policy` | enum | yes | `fail_fast`, `isolate_failures`, `collect_all`, `coordinator_decides`. |
| `completion_policy` | enum | yes | `root_and_no_unresolved`, `all_terminal`, `aggregate_threshold`, `coordinator_declares`. |
| `retry_propagation` | enum | yes | `none`, `inherit_defaults_on_spawn`, `coordinator_decides`. |
| `cancellation_propagation` | enum | yes | `cancel_all`, `cancel_unscheduled_only`, `let_active_finish`, `coordinator_decides`. |
| `budget_ids` | list<UUID> | no | Attached flow-scoped budgets. |
| `default_routing_hints` | structured object | no | Flow-level defaults for locality, capability preferences, isolation, or cost tier. |

##### Summary / derived state

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `public_flow_state` | enum | yes | Derived: `open`, `running`, `blocked`, `completed`, `failed`, `cancelled`. |
| `members_waiting` | u32 | yes | Count of members in execution `public_state = waiting`. |
| `members_delayed` | u32 | yes | Count of delayed members. |
| `members_rate_limited` | u32 | yes | Count of rate-limited members. |
| `members_waiting_children` | u32 | yes | Count of members blocked on dependencies. |
| `members_active` | u32 | yes | Count of active members. |
| `members_suspended` | u32 | yes | Count of suspended members. |
| `members_completed` | u32 | yes | Count of successful terminal members. |
| `members_failed` | u32 | yes | Count of failed terminal members. |
| `members_cancelled` | u32 | yes | Count of cancelled terminal members. |
| `members_expired` | u32 | yes | Count of expired terminal members. |
| `members_skipped` | u32 | yes | Count of skipped terminal members. |
| `aggregate_input_tokens` | u64 | yes | Sum across member executions. |
| `aggregate_output_tokens` | u64 | yes | Sum across member executions. |
| `aggregate_thinking_tokens` | u64 | yes | Sum across member executions. |
| `aggregate_cost_micros` | u64 | yes | Sum across member executions. |
| `aggregate_latency_ms` | u64 | yes | Sum across member executions. |
| `unresolved_dependency_count` | u64 | yes | Total unresolved required inbound edges across non-terminal members. |
| `last_summary_update_at` | unix ms | yes | When the derived summary was last refreshed. |

### Dependency Edge Definition

The flow container owns explicit dependency edges.

#### Edge object

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `edge_id` | UUID or deterministic hash | yes | Unique dependency edge identity. |
| `flow_id` | UUID | yes | Parent flow identity. |
| `upstream_execution_id` | UUID | yes | Source node. |
| `downstream_execution_id` | UUID | yes | Dependent node. |
| `dependency_kind` | enum | yes | `success_only` in v1. |
| `satisfaction_condition` | enum | yes | `all_required` in v1. |
| `data_passing_ref` | nullable string | no | Reference to upstream output/artifact/result that downstream may consume. |
| `created_at` | unix ms | yes | Creation time. |
| `created_by` | string | yes | Actor or coordinator that created the edge. |
| `edge_state` | enum | yes | `pending`, `active`, `satisfied`, `blocked_impossible`, `cancelled`. |

### Invariants

#### F1. Stable flow identity

Each flow has one stable `flow_id` for its lifetime. Dynamic expansion mutates membership and topology, not the flow’s identity.

Rationale:
- operators need one durable coordination handle
- policies and budgets attach to the flow object, not to one transient snapshot of its graph

#### F2. Execution belongs to coordination scope explicitly

Membership is explicit. An execution is in zero or one flow, and if it is in a flow, both the execution record and the flow membership index reflect that fact.

Rationale:
- flow membership cannot be guessed from parent pointers alone
- rootless flows and DAG edges require an explicit container

#### F3. Dependency-driven eligibility

A member execution with inbound dependencies must not become claimable until its required inbound dependencies are satisfied.

Rationale:
- dependency semantics are correctness, not presentation
- a DAG node running early corrupts the orchestration model

#### F4. Coordination is separate from execution identity

The flow container coordinates executions; it does not replace them. Waiting, suspension, lease ownership, and replay all remain execution primitives.

Rationale:
- the passive-container rule keeps one wait/reclaim/lease model
- rootless flows remain possible without inventing fake flow-runtime state

#### F5. Flow policy is visible

Failure propagation, completion rules, cancellation propagation, and routing/budget defaults must be inspectable on the flow object.

Rationale:
- “why did this branch get skipped?” and “why is this flow failed?” must be answered from durable policy state, not inferred from code

### Topology Classes

#### Tree

One parent per node except the root. Best-supported and simplest v1 path.

#### Chain

A linear ordered pipeline. Modeled as a specialized tree/DAG with one predecessor and one successor per middle node.

#### Fan-out / fan-in

One or more split points followed by joins. Still representable with explicit edges.

#### DAG

True DAG is in scope for v1. The most important dependency semantics are success-only and all-required inbound dependencies.

#### Dynamic expansion

Executions and edges may be added at runtime when `dynamic_expansion_enabled = true`. This is required for agentic decomposition, recursive orchestration, and adaptive fan-out.

### Dependency Model

#### Dependency kinds

V1:

- `success_only`

Meaning:

- a downstream node becomes eligible only when the upstream node completed successfully

Designed for later:

- any-of dependencies
- quorum dependencies
- threshold joins
- richer multi-edge semantics

#### Satisfaction condition

V1 aggregation is:

- `all_required`

Meaning:

- every required inbound edge must be satisfied before the downstream node becomes eligible

#### Data-passing reference

`data_passing_ref` is an optional pointer to the upstream output or artifact the downstream node expects to consume. It is metadata, not a transport channel.

### Eligibility Rules

A flow member execution is eligible to be claimed only when all of the following are true:

1. execution lifecycle is `runnable`
2. ownership state is `unowned`
3. all required inbound dependencies are satisfied
4. no required inbound dependency has become impossible under the flow failure policy
5. flow failure policy does not block the node
6. attached budgets and route/admission policies do not block the node

When dependencies remain unresolved, the execution must surface:

- `eligibility_state = blocked_by_dependencies`
- `blocking_reason = waiting_for_children`
- `public_state = waiting_children`

When required inbound dependencies can no longer be satisfied under success-only semantics, the downstream node becomes terminal:

- `terminal_outcome = skipped`

### Failure Policies

#### `fail_fast`

- any critical member failure fails the overall flow immediately
- further unscheduled members are blocked or cancelled by policy

#### `isolate_failures`

- default v1 behavior
- failing node affects only the dependent subgraph
- unrelated branches continue
- nodes whose required predecessors can no longer satisfy success-only dependencies become `skipped`

#### `collect_all`

- allow all branches to reach terminal states before deriving the aggregate flow outcome

#### `coordinator_decides`

- a coordinator/root execution interprets branch outcomes and declares flow-level result

Locked v1 default:

- dependency-local propagation, equivalent to `isolate_failures`

### Completion Model

Flow completion is derived, not guessed.

Supported completion policies:

#### `root_and_no_unresolved`

The flow is complete when:

- the root execution reached terminal success
- no non-terminal members remain
- no unresolved required dependencies remain

#### `all_terminal`

The flow is complete when all members are terminal, including `skipped` nodes.

#### `aggregate_threshold`

The flow is complete when a defined threshold across member outcomes is satisfied. This is designed for but not the primary v1 path.

#### `coordinator_declares`

The flow completes when the coordinator/root execution explicitly records the declaration and no policy violation remains.

Derived outcome notes:

- `skipped` nodes count as terminal
- a flow must not remain semantically open forever just because some descendants became impossible to run

### Dynamic Child Spawning

When `dynamic_expansion_enabled = true`, new member executions and new edges may be added at runtime.

Validation rules:

1. the new execution must be explicitly added to membership
2. all new edges must refer to members of the same flow
3. topology kind must permit the mutation
4. DAG mode must reject cycles
5. policy inheritance from flow to execution remains explicit and auditable
6. if the flow has an attached budget with `on_hard_limit = deny_child`, exhausted allowance rejects the spawn before membership is committed

### Cancellation Propagation

Flow-level cancellation is policy-controlled.

#### `cancel_all`

- issue cancellation to all members, including active ones
- `cancel_flow` always runs with operator-level privileges — active member cancellation bypasses lease validation unconditionally

#### `cancel_unscheduled_only`

- cancel members not yet active
- active members may finish or be cancelled separately

#### `let_active_finish`

- prevent new claims
- allow active members to finish
- move unclaimed runnable members to:
  - `eligibility_state = blocked_by_operator`
  - `blocking_reason = paused_by_policy`
- remove those unclaimed runnable members from eligible/runnable indexes
- when active members drain to zero, cancel the still-unclaimed blocked members
- mark the flow cancelled once active work drains

#### `coordinator_decides`

- a coordinator execution decides which branches to cancel or allow to finish

The passive flow container does not suspend or wait during cancellation. It issues execution-level control actions and derives state from member outcomes.

### Flow State Model

`public_flow_state` is derived from member execution states and flow policy.

#### `open`

- flow exists
- no terminal aggregate outcome yet
- no member active yet or work has not materially started

#### `running`

- at least one member is `active`

#### `blocked`

- no member is active
- the flow is not terminal
- at least one non-terminal member is delayed, suspended, waiting on children, rate-limited, or otherwise blocked

#### `completed`

- completion policy is satisfied
- aggregate outcome is successful

#### `failed`

- failure policy declares overall failure

#### `cancelled`

- flow cancellation policy or operator action has produced a cancelled aggregate outcome

### Operations

| Operation | Class | Semantics |
| --- | --- | --- |
| `create_flow(flow_spec)` | A | Creates the flow core record and initial structural metadata. |
| `add_execution_to_flow(flow_id, execution_id, member_spec)` | A | Adds explicit membership, increments `graph_revision` (membership is a structural mutation that can invalidate concurrent cycle check snapshots), and applies any flow defaults that are inherited by the execution. If the flow has an attached budget whose hard-limit action is `deny_child` and the remaining allowance is exhausted, the add/spawn is rejected. |
| `add_dependency(flow_id, upstream_execution_id, downstream_execution_id, edge_spec)` | A | Validates topology and creates a dependency edge; also updates child-local dependency gating. |
| `resolve_dependency(flow_id, edge_id, upstream_outcome)` | A | Applies one upstream outcome to one edge and updates downstream eligibility or impossibility atomically on the child partition. |
| `get_flow(flow_id)` | C | Returns the flow core record. |
| `get_flow_graph(flow_id)` | C | Returns members and edge topology. |
| `get_flow_summary(flow_id)` | C | Returns derived state, counts, unresolved dependency totals, and aggregate usage. |
| `cancel_flow(flow_id, policy_override?)` | A | Starts flow-level cancellation according to configured or overridden propagation policy. |
| `attach_budget_to_flow(flow_id, budget_id)` | A | Attaches a budget to the flow policy scope. |

## Error Model

| Error | Meaning |
| --- | --- |
| `flow_not_found` | The flow does not exist. |
| `execution_not_in_flow` | The referenced execution is not a member of the flow. |
| `execution_already_in_flow` | Tried to add an execution that already belongs to a flow. |
| `duplicate_flow_membership_entry` | Membership already exists for this flow and execution. |
| `invalid_dependency` | Edge endpoints or dependency kind are invalid. |
| `dependency_already_exists` | The same edge already exists. |
| `cycle_detected` | Adding the edge would create a cycle in DAG mode. |
| `flow_mutation_not_allowed` | Mutation attempted when flow policy or terminal aggregate state forbids it. |
| `flow_policy_violation` | Requested mutation conflicts with flow failure/completion/cancellation policy. |
| `dynamic_expansion_disabled` | Runtime addition attempted while dynamic expansion is disabled. |
| `dependency_resolution_mismatch` | Tried to resolve an edge against an upstream outcome that does not match its current state or source. |

## Valkey Data Model

### Partitioning model

This RFC intentionally separates:

1. **flow-structural truth** on the flow partition
2. **runtime eligibility truth** on each child execution partition

Why:

- flow structure is keyed by `flow_id`
- execution claimability is keyed by `execution_id`
- arbitrary flow members may live on different execution partitions
- a single Lua script cannot atomically mutate arbitrary flow and execution partitions at once in Valkey Cluster

Therefore:

- topology and cycle validation are authoritative on the flow partition
- child eligibility gating is authoritative on the child execution partition
- cross-partition operations use idempotent mutation grants and flow events

### Flow partition

`flow_partition_id = hash(flow_id) % num_partitions`

Keys on the flow partition:

- `ff:flow:{fp:N}:<flow_id>:core`
- `ff:flow:{fp:N}:<flow_id>:members`
- `ff:flow:{fp:N}:<flow_id>:member:<execution_id>`
- `ff:flow:{fp:N}:<flow_id>:edge:<edge_id>`
- `ff:flow:{fp:N}:<flow_id>:out:<upstream_execution_id>`
- `ff:flow:{fp:N}:<flow_id>:in:<downstream_execution_id>`
- `ff:flow:{fp:N}:<flow_id>:events`
- `ff:flow:{fp:N}:<flow_id>:summary`
- `ff:flow:{fp:N}:<flow_id>:grant:<mutation_id>`

### Execution-local dependency state

Keys on the downstream execution partition:

- `ff:exec:{p:N}:<execution_id>:deps:meta`
- `ff:exec:{p:N}:<execution_id>:dep:<edge_id>`
- `ff:exec:{p:N}:<execution_id>:deps:unresolved`

#### `deps:meta` fields

- `flow_id`
- `unsatisfied_required_count`
- `impossible_required_count`
- `last_dependency_update_at`
- `last_flow_graph_revision`

#### `dep:<edge_id>` fields

- `edge_id`
- `flow_id`
- `upstream_execution_id`
- `downstream_execution_id`
- `dependency_kind`
- `state` (`unsatisfied`, `satisfied`, `impossible`, `cancelled`)
- `data_passing_ref`
- `last_resolved_at`

### Flow core record

`ff:flow:{fp:N}:<flow_id>:core` stores:

- identity fields
- structure fields
- policy fields
- derived `public_flow_state`
- summary counters or pointers to a summary projection

Summary fields are derived and may lag slightly behind member-execution truth. They are fast-read projections, not the sole source of correctness.

### Membership set

`ff:flow:{fp:N}:<flow_id>:members`

- set of member `execution_id`s

Optional member detail:

`ff:flow:{fp:N}:<flow_id>:member:<execution_id>`

- `added_at`
- `added_by`
- `is_root`
- `parent_execution_id` if applicable

### Edge storage

`ff:flow:{fp:N}:<flow_id>:edge:<edge_id>`

- full edge record

Adjacency sets:

- `ff:flow:{fp:N}:<flow_id>:out:<upstream_execution_id>` -> set of `edge_id`
- `ff:flow:{fp:N}:<flow_id>:in:<downstream_execution_id>` -> set of `edge_id`

These are authoritative for topology inspection and cycle validation.

### Add dependency: structural validation + child gating

External API semantics:

- `add_dependency` is Class A from the engine’s point of view

Implementation strategy:

1. validate and stage the edge on the flow partition
2. issue an idempotent mutation grant
3. consume the grant on the child execution partition to update dependency gating
4. finalize the edge as active

This keeps structural truth and runtime truth consistent without pretending a single cross-slot Lua script can do impossible work.

#### `stage_dependency_edge.lua` on flow partition

Responsibilities:

- verify both executions are members of the flow
- verify topology kind allows the edge
- run cycle check in DAG mode
- create `edge:<edge_id>` with `edge_state = pending`
- increment `graph_revision`
- create `grant:<mutation_id>` with child target info

Cycle check note:

- for v1, cycle detection may be done in control plane against the flow adjacency snapshot and then committed by script with `graph_revision` optimistic validation
- the script must still reject stale graph revisions

#### `apply_dependency_to_child.lua` on child execution partition

Pseudocode:

```lua
-- KEYS: exec_core, deps_meta, unresolved_set, dep_hash
-- ARGV: flow_id, edge_id, upstream_execution_id, graph_revision,
--       dependency_kind, data_passing_ref, now_ms

local core = redis.call("HGETALL", KEYS[1])
assert_execution_exists(core)
assert_flow_membership(core, ARGV.flow_id)

if redis.call("EXISTS", KEYS[4]) == 1 then
  return ok("already_applied")
end

redis.call("HSET", KEYS[4],
  "edge_id", ARGV.edge_id,
  "flow_id", ARGV.flow_id,
  "upstream_execution_id", ARGV.upstream_execution_id,
  "downstream_execution_id", core.execution_id,
  "dependency_kind", ARGV.dependency_kind,
  "state", "unsatisfied",
  "data_passing_ref", ARGV.data_passing_ref or "",
  "last_resolved_at", ""
)

redis.call("SADD", KEYS[3], ARGV.edge_id)
local unresolved = redis.call("HINCRBY", KEYS[2], "unsatisfied_required_count", 1)
redis.call("HSET", KEYS[2],
  "flow_id", ARGV.flow_id,
  "last_flow_graph_revision", ARGV.graph_revision,
  "last_dependency_update_at", ARGV.now_ms
)

-- Partial update: only eligibility/blocking change. Other state vector dimensions unchanged.
if core.lifecycle_phase == "runnable" and core.terminal_outcome == "none" then
  redis.call("HSET", KEYS[1],
    "eligibility_state", "blocked_by_dependencies",
    "blocking_reason", "waiting_for_children",
    "public_state", "waiting_children"
  )
end

return ok(unresolved)
```

### Resolve dependency

When an upstream execution reaches a relevant terminal outcome, the engine uses the flow partition’s outgoing adjacency to drive child-local updates.

#### `resolve_dependency.lua` on child execution partition

Pseudocode:

```lua
-- KEYS: exec_core, deps_meta, unresolved_set, dep_hash, runnable_index, terminal_zset
-- ARGV: edge_id, upstream_outcome, now_ms

local dep = redis.call("HGETALL", KEYS[4])
if not dep.edge_id then
  return err("invalid_dependency")
end

if dep.state == "satisfied" or dep.state == "impossible" then
  return ok("already_resolved")
end

if ARGV.upstream_outcome == "success" then
  redis.call("HSET", KEYS[4], "state", "satisfied", "last_resolved_at", ARGV.now_ms)
  redis.call("SREM", KEYS[3], ARGV.edge_id)
  local remaining = redis.call("HINCRBY", KEYS[2], "unsatisfied_required_count", -1)

  local core = redis.call("HGETALL", KEYS[1])
  if remaining == 0
     and core.lifecycle_phase == "runnable"
     and core.ownership_state == "unowned"
     and core.terminal_outcome == "none"
     and core.eligibility_state == "blocked_by_dependencies" then
    redis.call("HSET", KEYS[1],
      "eligibility_state", "eligible_now",
      "blocking_reason", "waiting_for_worker",
      "attempt_state", "pending_first_attempt",
      "public_state", "waiting",
      "last_tx", ARGV.now_ms, "last_mut", ARGV.now_ms
    )
    local priority = tonumber(core.priority or "0")
    local created_at_ms = tonumber(core.created_at or "0")
    local score = 0 - (priority * 1000000000000) + created_at_ms
    redis.call("ZADD", KEYS[5], score, core.execution_id)
  end

  return ok("satisfied")
end

-- success_only default: failed/cancelled/expired/skipped upstream makes this edge impossible
redis.call("HSET", KEYS[4], "state", "impossible", "last_resolved_at", ARGV.now_ms)
redis.call("SREM", KEYS[3], ARGV.edge_id)
redis.call("HINCRBY", KEYS[2], "unsatisfied_required_count", -1)
redis.call("HINCRBY", KEYS[2], "impossible_required_count", 1)

local core = redis.call("HGETALL", KEYS[1])
if core.terminal_outcome == "none" then
  redis.call("HSET", KEYS[1],
    "lifecycle_phase", "terminal",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "terminal_outcome", "skipped",
    "attempt_state", "none",
    "public_state", "skipped",
    "completed", ARGV.now_ms,
    "last_tx", ARGV.now_ms, "last_mut", ARGV.now_ms
  )
  redis.call("ZADD", KEYS[6], ARGV.now_ms, core.execution_id)
end

return ok("impossible")
```

### Eligibility check

The claim path must not reach across the flow partition. It checks child-local dependency state instead.

#### `evaluate_flow_eligibility.lua`

Pseudocode:

```lua
-- KEYS: exec_core, deps_meta
local core = redis.call("HGETALL", KEYS[1])
local deps = redis.call("HGETALL", KEYS[2])

if core.lifecycle_phase ~= "runnable" then
  return ok("not_runnable")
end
if core.ownership_state ~= "unowned" then
  return ok("owned")
end
if core.terminal_outcome ~= "none" then
  return ok("terminal")
end

local unresolved = tonumber(deps.unsatisfied_required_count or "0")
local impossible = tonumber(deps.impossible_required_count or "0")

if impossible > 0 then
  return ok("impossible")
end
if unresolved > 0 then
  return ok("blocked_by_dependencies")
end

return ok("eligible")
```

### Flow summary projection

Flow summary counts and aggregate usage should be maintained by a projector consuming execution state changes and flow events.

Why projection:

- member executions live on arbitrary execution partitions
- recomputing counts on every read is too expensive
- a single flow summary hash provides fast inspection

Correctness boundary:

- summary is not authoritative for member claimability
- member execution state and child-local dependency state remain authoritative

## Interactions with Other Primitives

- **RFC-001 Execution**: member eligibility, waiting-children rendering, skipped outcomes, and execution state vectors are the foundation of flow behavior.
- **RFC-002 Attempt**: attempts remain execution-local; flow coordinates executions, not attempts directly.
- **RFC-003 Lease**: the flow container never owns leases. Active work remains execution-owned.
- **RFC-008 Budget**: budgets may attach at flow scope and influence member admission and derived blocked state.

## V1 Scope

In v1:

- real flow container with optional root execution
- tree, chain, fan-out/fan-in, and DAG support
- success-only dependencies
- all-required inbound dependency aggregation
- dependency-local failure propagation default
- terminal `skipped` for impossible downstream nodes
- dynamic child spawning when enabled
- flow summary and topology inspection

**Known v1 limitation — large fan-out burst:** When a parent execution with N outgoing edges completes, the engine calls `resolve_dependency.lua` N times (once per downstream child, potentially across many `{p:N}` partitions). If all N children have this as their only dependency, all become eligible simultaneously. For N=10,000: ~10,000 Lua calls across ~256 partitions ≈ 40 per partition ≈ 1-2ms per partition. The burst is acceptable for v1. Natural backpressure exists: the scheduler's weighted round-robin fairness rate-limits claim-grant issuance, so the claim flood is paced. Future mitigation: batch by partition (group edges by downstream `{p:N}` and resolve multiple edges per Lua call) and stagger resolution in batches of 100-500 with small yields.

Designed for later but not required in v1:

- any-of dependencies
- quorum/threshold joins
- richer multi-edge semantics
- multi-flow membership
- coordinator-authored custom aggregate completion logic beyond the declared policy set

## Open Questions

Only genuinely unresolved questions remain here.

1. Should `aggregate_threshold` completion be a first-class v1 policy or remain designed-for until a concrete product requires it?
2. Should the flow summary projector be best-effort polling, event-driven, or hybrid by default?
3. How much edge-level data-passing metadata should be standardized versus left as opaque references?
4. Replay of a completed upstream execution does not automatically un-resolve already-satisfied dependency edges in v1. If replay semantics need to reopen downstream eligibility, that will require an explicit graph-reconciliation model in a later RFC.

## References

- Pre-RFC: `flowfabric_use_cases_and_primitives (2).md`
- Related RFCs: RFC-001, RFC-002, RFC-003, RFC-008
