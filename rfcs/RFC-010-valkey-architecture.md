# RFC-010: Valkey-Native Architecture Mapping

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-14
**Pre-RFC Reference:** All prior RFCs (001–009)

---

## Summary

This RFC is the cross-cutting architecture document that maps all FlowFabric primitives onto one implementable Valkey backend. It consolidates every Valkey key, every partition scheme, every cross-partition operation, and every background scanner into a single reference. Implementers should read RFC-001 through RFC-009 for semantic detail; this RFC is the physical blueprint.

---

## Part 1: Data Model, Partition Model, and Cross-Partition Operations

### 1. Complete Key Schema Registry

Every `ff:*` key in the FlowFabric Valkey backend, extracted from RFCs 001–009. Canonical names as of RFC-003/007 final revision.

#### 1.1 Execution Partition Keys — `{p:N}`

All keys below share the hash tag `{p:N}` where `N = crc16(execution_id) % num_partitions`. This collocates them on one Valkey Cluster shard, enabling atomic Lua scripts.

##### Execution Core (RFC-001)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:core` | HASH | Created on `create_execution`. Deleted on retention purge. | Authoritative execution record. All 6 state vector dimensions + lease mirror fields + timestamps + accounting. |
| `ff:exec:{p:N}:<execution_id>:payload` | STRING | Created with execution. Deleted on retention purge. | Opaque input payload. Separated from core to avoid loading large blobs on state reads. |
| `ff:exec:{p:N}:<execution_id>:result` | STRING | Created on `complete_execution`. Deleted on retention purge. | Opaque result payload. |
| `ff:exec:{p:N}:<execution_id>:policy` | STRING | Created with execution. Deleted on retention purge. | JSON-encoded ExecutionPolicySnapshot. |
| `ff:exec:{p:N}:<execution_id>:tags` | HASH | Created with execution if tags provided. Deleted on retention purge. | User-supplied key-value labels. |

##### Lease (RFC-003)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:lease:current` | HASH | Created on `acquire_lease`. DEL on lease release (complete/fail/suspend/delay). PEXPIREAT set to `expires_at + grace_ms` for cleanup. | Full lease object. Authoritative ownership is on `exec:core`; this is detailed audit state. |
| `ff:exec:{p:N}:<execution_id>:lease:history` | STREAM | Created on first lease event. Trimmed with `MAXLEN ~`. Deleted on retention purge. | Append-only lease events: acquired, renewed, released, expired, revoked, reclaimed. Each entry includes `lease_id`, `lease_epoch`, `attempt_index`, `attempt_id`, `worker_id`, `worker_instance_id`, `ts`. |
| `ff:exec:{p:N}:<execution_id>:claim_grant` | HASH | Created by scheduler (`issue_claim_grant`). DEL on consumption by `acquire_lease`. TTL ~5s. | Ephemeral. Bridges scheduler decision to atomic claim. Fields: `worker_id`, `worker_instance_id`, `lane_id`, `capability_hash`, `grant_expires_at`, `route_snapshot_json`, `admission_summary`. |

##### Attempt (RFC-002)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:attempts` | ZSET | Created on first attempt. Deleted on retention purge. | Attempt index. Member: `attempt_index`, Score: `attempt_index` (integer). Provides monotonic ordering and O(1) latest-attempt lookup via `ZREVRANGE 0 0`. |
| `ff:attempt:{p:N}:<execution_id>:<attempt_index>` | HASH | Created atomically with attempt start or creation. Deleted on retention purge. | Per-attempt detail: identity, timing, outcome, ownership, lineage, AI context. |
| `ff:attempt:{p:N}:<execution_id>:<attempt_index>:usage` | HASH | Created with attempt (all counters = 0). Deleted on retention purge. | Hot-path usage counters (HINCRBY target). Separated from attempt detail to avoid contention between usage increments (Class B) and state transitions (Class A). |
| `ff:attempt:{p:N}:<execution_id>:<attempt_index>:policy` | HASH | Created with attempt. Deleted on retention purge. | Frozen AttemptPolicySnapshot: timeout, provider, model, fallback_index, route, pricing. |

##### Stream (RFC-006)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:stream:{p:N}:<execution_id>:<attempt_index>` | STREAM | Lazy-created on first `append_frame`. Closed atomically with attempt termination. Deleted on retention purge or time-based cleanup. | Attempt-scoped output frames via XADD. Valkey Stream provides total order, XRANGE for replay, XREAD BLOCK for tailing. Subject to MAXLEN trim. |
| `ff:stream:{p:N}:<execution_id>:<attempt_index>:meta` | HASH | Created atomically with first frame append. Deleted with stream. | Stream metadata: `created_at`, `closed_at`, `closed_reason`, `durability_mode`, `retention_maxlen`, `last_sequence`, `frame_count`. |

##### Suspension and Waitpoint (RFC-004)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:suspension:current` | HASH | Created on `suspend_execution`. Closed (fields updated) on resume/cancel/timeout. PEXPIREAT after closure for cleanup. | Current suspension episode. Fields include `suspension_id`, `waitpoint_id`, `waitpoint_key`, `reason_code`, `resume_condition_json`, `resume_policy_json`, `continuation_metadata_pointer`. |
| `ff:wp:{p:N}:<waitpoint_id>` | HASH | Created on `create_pending_waitpoint` or `suspend_execution`. Closed on resume/expiry/cancel. | Waitpoint record. State: `pending` → `active` → `satisfied` → `closed` (or `expired`). |
| `ff:wp:{p:N}:<waitpoint_id>:signals` | STREAM | Created on first accepted signal. Trimmed with `MAXLEN ~`. | Per-waitpoint signal history. Authoritative ordered record of accepted signals. Shared definition with RFC-005. |
| `ff:wp:{p:N}:<waitpoint_id>:condition` | HASH | Created with waitpoint activation. Updated atomically on signal match. | Resume condition evaluation state. Tracks which matchers are satisfied and by which signal_id. RFC-005 owns evaluation; RFC-004 owns schema. |

##### Signal (RFC-005)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:signal:{p:N}:<signal_id>` | HASH | Created on signal acceptance. Deleted on execution retention purge. | Signal record: identity, target, name, category, source, effect, timestamps. |
| `ff:signal:{p:N}:<signal_id>:payload` | STRING | Created with signal if payload present. Deleted on retention purge. | Opaque signal payload (max 64KB v1). Separated from signal hash to avoid loading blobs on reads. |
| `ff:exec:{p:N}:<execution_id>:signals` | ZSET | Created on first signal for execution. Deleted on retention purge. | Per-execution signal index. Member: `signal_id`, Score: `accepted_at` (ms). Cross-waitpoint signal history. |
| `ff:sigdedup:{p:N}:<waitpoint_id>:<idempotency_key>` | STRING | Created on idempotent signal acceptance (`SET NX`). TTL: `signal_dedup_window` (e.g. 24h). | Idempotency guard. Value: `signal_id`. Auto-expires. |

##### Flow Dependency — Execution-Local (RFC-007)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:deps:meta` | HASH | Created when first dependency edge is applied to this execution. Deleted on retention purge. | Dependency summary: `flow_id`, `unsatisfied_required_count`, `impossible_required_count`, `last_dependency_update_at`, `last_flow_graph_revision`. |
| `ff:exec:{p:N}:<execution_id>:dep:<edge_id>` | HASH | Created by `apply_dependency_to_child`. Deleted on retention purge. | Per-edge local state: `state` (unsatisfied/satisfied/impossible/cancelled), `upstream_execution_id`, `dependency_kind`, `data_passing_ref`. |
| `ff:exec:{p:N}:<execution_id>:deps:unresolved` | SET | Created with first dependency. Members removed as edges resolve. Deleted on retention purge. | Set of unresolved `edge_id`s. Fast check for "are all dependencies satisfied?" |

##### Waitpoint Discovery + Suspension History (RFC-004)

| Key Pattern | Type | Lifecycle | Notes |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:waitpoints` | SET | **Required.** SADD on every `suspend_execution`. Deleted on retention purge. | Set of all `waitpoint_id`s ever created for this execution. Used by the cleanup cascade to discover and DEL all waitpoint-related keys (hash, :signals stream, :condition hash). Without this, old waitpoints from prior suspension episodes are orphaned. |
| `ff:exec:{p:N}:<execution_id>:suspensions` | STREAM or ZSET | Optional. Append on each suspension episode close. | Historical suspension records for executions with multiple suspension episodes. |

##### Partition-Local Indexes — `{p:N}`

| Key Pattern | Type | RFC | Score / Member | Notes |
|---|---|---|---|---|
| `ff:idx:{p:N}:lane:<lane_id>:eligible` | ZSET | 001, 003, 009 | Member: `execution_id`. Score: `-(priority * 1_000_000_000_000) + created_at_ms`. | Executions with `eligible_now`. ZPOPMIN gives highest-priority, oldest first. Canonical name per RFC-003 revision. |
| `ff:idx:{p:N}:lane:<lane_id>:delayed` | ZSET | 001 | Member: `execution_id`. Score: `delay_until` (ms). | Promotion loop: `ZRANGEBYSCORE -inf <now>` moves to eligible. |
| `ff:idx:{p:N}:lane:<lane_id>:active` | ZSET | 003 | Member: `execution_id`. Score: `lease_expires_at` (ms). | Partition-local active execution index. Updated by acquire/release. |
| `ff:idx:{p:N}:lane:<lane_id>:terminal` | ZSET | 001 | Member: `execution_id`. Score: `completed_at` (ms). | Retention scanning and historical queries. |
| `ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies` | ZSET | 001 | Member: `execution_id`. Score: `created_at` (ms). | Blocked on upstream flow executions. |
| `ff:idx:{p:N}:lane:<lane_id>:blocked:budget` | ZSET | 001 | Member: `execution_id`. | Blocked by budget exhaustion. |
| `ff:idx:{p:N}:lane:<lane_id>:blocked:quota` | ZSET | 001 | Member: `execution_id`. | Blocked by quota/rate-limit. |
| `ff:idx:{p:N}:lane:<lane_id>:blocked:route` | ZSET | 001 | Member: `execution_id`. | No capable/available worker. |
| `ff:idx:{p:N}:lane:<lane_id>:blocked:operator` | ZSET | 001 | Member: `execution_id`. | Operator hold. |
| `ff:idx:{p:N}:lane:<lane_id>:suspended` | ZSET | 001 | Member: `execution_id`. Score: `suspension_timeout_at` (ms) or MAX. | Per-lane suspended set. |
| `ff:idx:{p:N}:lease_expiry` | ZSET | 001, 003 | Member: `execution_id`. Score: `lease_expires_at` (ms). | Cross-lane within partition. Reclaim scanner target. |
| `ff:idx:{p:N}:worker:<worker_instance_id>:leases` | SET | 001, 003 | Member: `execution_id`. | Per-worker lease set within partition. Operator drain aid. Best-effort — authoritative ownership is on exec core. |
| `ff:idx:{p:N}:suspension_timeout` | ZSET | 004 | Member: `execution_id`. Score: `timeout_at` (ms). | Suspension timeout scanner target. Cross-lane within partition. |
| `ff:idx:{p:N}:pending_waitpoint_expiry` | ZSET | 004 | Member: `waitpoint_id`. Score: `expires_at` (ms). | Pending waitpoint cleanup scanner target. |
| `ff:idx:{p:N}:all_executions` | SET | 010 | Member: `execution_id`. | All execution IDs in this partition. SADD on `create_execution`, SREM on retention purge. Used by generalized index reconciler (§6.17) to iterate executions without SCAN. O(N_executions) instead of O(N_total_keys_on_node). |

#### 1.2 Flow-Structural Partition Keys — `{fp:N}`

All keys below share the hash tag `{fp:N}` where `N = crc16(flow_id) % num_flow_partitions`. These are the authoritative topology and membership records.

| Key Pattern | Type | RFC | Lifecycle | Notes |
|---|---|---|---|---|
| `ff:flow:{fp:N}:<flow_id>:core` | HASH | 007 | Created on `create_flow`. Deleted on flow retention purge. | Flow identity, structure, policy, derived `public_flow_state`, summary counters. |
| `ff:flow:{fp:N}:<flow_id>:members` | SET | 007 | Created with flow. Members added/removed on membership mutations. | Authoritative set of member `execution_id`s. |
| `ff:flow:{fp:N}:<flow_id>:member:<execution_id>` | HASH | 007 | Created on `add_execution_to_flow`. | Optional per-member detail: `added_at`, `added_by`, `is_root`, `parent_execution_id`. |
| `ff:flow:{fp:N}:<flow_id>:edge:<edge_id>` | HASH | 007 | Created by `stage_dependency_edge`. | Full edge record: endpoints, kind, state, data_passing_ref. |
| `ff:flow:{fp:N}:<flow_id>:out:<upstream_execution_id>` | SET | 007 | Created/updated on edge addition. | Outgoing adjacency: set of `edge_id`s from this upstream node. |
| `ff:flow:{fp:N}:<flow_id>:in:<downstream_execution_id>` | SET | 007 | Created/updated on edge addition. | Incoming adjacency: set of `edge_id`s into this downstream node. |
| `ff:flow:{fp:N}:<flow_id>:events` | STREAM | 007 | Created on first flow event. Trimmed. | Flow lifecycle event stream. |
| `ff:flow:{fp:N}:<flow_id>:summary` | HASH | 007 | Created with flow. Updated by summary projector. | Derived member state counts and aggregate usage. Fast-read projection, not sole source of truth. |
| `ff:flow:{fp:N}:<flow_id>:grant:<mutation_id>` | HASH | 007 | Created by `stage_dependency_edge`. Consumed by `apply_dependency_to_child`. Short TTL. | Idempotent cross-partition mutation grant for dependency edge application. |

#### 1.3 Budget Partition Keys — `{b:M}`

Budget keys use `{b:M}` where `M = crc16(budget_id) % num_budget_partitions` for flow/lane/tenant-scoped budgets. Execution-scoped budgets colocated with the execution use `{p:N}` instead (same key patterns, different hash tag).

| Key Pattern | Type | RFC | Lifecycle | Notes |
|---|---|---|---|---|
| `ff:budget:{b:M}:<budget_id>` | HASH | 008 | Created on `create_budget`. Deleted on budget removal. | Budget definition: scope, enforcement policy, breach counters, metadata. |
| `ff:budget:{b:M}:<budget_id>:limits` | HASH | 008 | Created with budget. Updated on `override_budget`. | Hard and soft limits per dimension. Fields: `hard:<dim>`, `soft:<dim>`. Separated from definition for clean reads. |
| `ff:budget:{b:M}:<budget_id>:usage` | HASH | 008 | Created with budget (all zeroes). Updated by `report_usage` (HINCRBY). Reset on budget reset. | Hot-path usage counters. Separated for HINCRBY contention isolation. |
| `ff:budget_attach:{b:M}:<scope_type>:<scope_id>` | SET | 008 | Created on `attach_budget`. Members added/removed. | Forward index: scope → attached budget_ids. |
| `ff:budget:{b:M}:<budget_id>:executions` | SET | 008 | Members added on execution creation/attachment. | Reverse index: budget → attached execution_ids. |
| `ff:idx:{b:M}:budget_resets` | ZSET | 008 | Updated on budget creation, reset, and override. Member: `budget_id`. Score: `next_reset_at` (ms). | Per-partition budget reset schedule. Scanner target. Colocated with budget keys on `{b:M}` — ZADD is atomic with budget creation. |

**Note on execution-scoped budgets:** When `scope_type = execution`, the budget keys use `{p:N}` (the execution's partition tag) instead of `{b:M}`. This enables atomic `report_usage + breach_check + execution_state_update` in one Lua script on the execution partition.

#### 1.4 Quota Partition Keys — `{q:K}`

Quota keys use `{q:K}` where `K = crc16(quota_scope_id) % num_quota_partitions`.

| Key Pattern | Type | RFC | Lifecycle | Notes |
|---|---|---|---|---|
| `ff:quota:{q:K}:<quota_policy_id>` | HASH | 008 | Created on `create_quota_policy`. | Quota policy definition: scope, rate limits, concurrency caps, breach actions. |
| `ff:quota:{q:K}:<quota_policy_id>:window:<dimension>` | ZSET | 008 | Entries added on admission. Stale entries removed by `ZREMRANGEBYSCORE` on each check. | Sliding window. Member: `<execution_id>:<timestamp_ms>`. Score: `timestamp_ms`. |
| `ff:quota:{q:K}:<quota_policy_id>:concurrency` | STRING | 008 | Created on first INCR. INCR on lease acquire, DECR on lease release (async). | Active concurrency counter. Subject to drift; reconciled periodically. |
| `ff:quota_attach:{q:K}:<scope_type>:<scope_id>` | SET | 008 | Created on `attach_quota_policy`. | Forward index: scope → attached quota_policy_ids. |

#### 1.5 Global Keys (No Hash Tag)

These keys have no hash tag and land on a shard determined by the full key string. They are few, read-heavy, and written infrequently — acceptable for global placement.

| Key Pattern | Type | RFC | Lifecycle | Notes |
|---|---|---|---|---|
| `ff:lane:<lane_id>:config` | HASH | 009 | Created on lane creation. Updated on lane config changes. | Lane configuration: state, default policies, scheduling weight, max concurrency. |
| `ff:lane:<lane_id>:counts` | HASH | 009 | Created with lane. Updated periodically by background aggregator. | Derived cached counts by public_state. Not authoritative — ZCARD on partition sets is authoritative but slower. |
| `ff:worker:<worker_instance_id>` | HASH | 009 | Created on `register_worker`. TTL based on `last_heartbeat_at + worker_ttl_ms`. Auto-expires on heartbeat failure. | Worker registration: capabilities, status, capacity, heartbeat. |
| `ff:idx:workers:cap:<key>:<value>` | SET | 009 | Updated atomically with worker registration. Members removed on deregistration or TTL expiry. | Capability index: which workers have a given capability. Member: `worker_instance_id`. |
| `ff:sched:fairness:lane:<lane_id>:deficit` | STRING | 009 | Created by scheduler. TTL: `fairness_window_ms * 2`. | Ephemeral. Weighted deficit for cross-lane round-robin. Rebuilt from queue depths on restart. |
| `ff:sched:fairness:tenant:<namespace>:lane:<lane_id>:claims` | STRING | 009 | Created by scheduler. TTL: `fairness_window_ms`. | Ephemeral. Tenant claim count in current fairness window. |

#### 1.6 Cross-Partition Secondary Indexes (Eventually Consistent)

These indexes are maintained by best-effort writes after the atomic transition succeeds on the primary partition. They cannot participate in atomic Lua scripts. Staleness is acceptable; queries against them should be treated as approximate.

| Key Pattern | Type | RFC | Notes |
|---|---|---|---|
| `ff:ns:<namespace>:executions` | ZSET | 001 | All executions in a namespace. Member: `execution_id`. Score: `created_at`. |
| `ff:idem:<namespace>:<idempotency_key>` | STRING | 001 | Execution idempotency dedup. Value: `execution_id`. TTL: `dedup_window`. |
| `ff:tag:<namespace>:<key>:<value>` | SET | 001 | Tag-based search index. Member: `execution_id`. |

---

### 2. Partition Model

#### 2.1 Partition Families

FlowFabric uses four partition families plus a global tier. Each family uses a distinct hash tag prefix so that keys from different families never collide in Valkey Cluster slot assignment, even with the same numeric partition ID.

| Family | Hash Tag | Assignment | Stored On | Purpose |
|---|---|---|---|---|
| Execution | `{p:N}` | `crc16(execution_id) % num_partitions` | Execution core record (`partition_id` field) | All per-execution keys: core, lease, attempt, stream, suspension, waitpoint, signal, dependency-local, partition-local indexes. |
| Flow-structural | `{fp:N}` | `crc16(flow_id) % num_flow_partitions` | Flow core record (`flow_partition_id` field) | Flow topology: core, members, edges, adjacency, events, summary, mutation grants. |
| Budget | `{b:M}` | `crc16(budget_id) % num_budget_partitions` | Budget definition record | Budget definition, limits, usage, attachments. Exception: execution-scoped budgets use `{p:N}`. |
| Quota | `{q:K}` | `crc16(quota_scope_id) % num_quota_partitions` | Quota policy record | Quota policy, sliding windows, concurrency counters, attachments. |
| Global | (none) | Valkey hashes the full key string | N/A | Lane config, worker registration, capability indexes, fairness state, budget reset schedule. |

#### 2.2 Partition Assignment

Partition assignment is computed **exactly once** at creation time and **never recomputed**. The partition ID is stored on the primary record and reused by every subsequent operation.

##### Algorithm

```
partition_id = crc16(entity_id_bytes) % num_partitions_for_family
```

`crc16` is the same CRC-16/CCITT function Valkey Cluster uses internally for slot assignment. Using the same function means the partition distribution roughly mirrors slot distribution, avoiding pathological clustering.

##### Where partition IDs are stored

| Entity | Field | Set When | Immutable? |
|---|---|---|---|
| Execution | `partition_id` on `ff:exec:{p:N}:...:core` | `create_execution` | Yes |
| Flow | `flow_partition_id` on `ff:flow:{fp:N}:...:core` | `create_flow` | Yes |
| Budget | Derived from `budget_id` at key construction time | `create_budget` | Yes (budget_id is stable) |
| Quota | Derived from `quota_scope_id` at key construction time | `create_quota_policy` | Yes |

##### Key construction

Every key is constructed by the caller before invoking a Lua script. The caller reads the partition ID from the entity record (or computes it for new entities) and assembles the `{p:N}` / `{fp:N}` / `{b:M}` / `{q:K}` prefix. The Lua script itself never computes partition assignments.

#### 2.3 Partition Count Recommendations — V1

| Family | Recommended V1 Count | Rationale |
|---|---|---|
| Execution (`{p:N}`) | 256 | Balances per-partition key density against shard spread. 256 partitions on a 3-shard cluster means ~85 partitions per shard. Sufficient for millions of executions. Scale to 1024+ by redeployment if needed. |
| Flow (`{fp:N}`) | 64 | Flows are fewer and shorter-lived than executions. 64 partitions is ample for tens of thousands of concurrent flows. |
| Budget (`{b:M}`) | 32 | Budgets are few (hundreds to low thousands). 32 partitions prevents hot-partition issues while keeping the namespace small. |
| Quota (`{q:K}`) | 32 | Similar cardinality to budgets. Quota policies are per-lane/tenant, not per-execution. |

**Partition count is fixed at deployment time.** Changing it requires a coordinated migration of all stored `partition_id` values and key names. V1 does not support online repartitioning. Choose conservatively — too many partitions is cheaper than too few (more sorted sets, but each is smaller and faster).

**Valkey Cluster slot mapping:** 16,384 Valkey Cluster slots are shared across all families. With the recommended counts (256 + 64 + 32 + 32 = 384 distinct hash tags), each hash tag maps to one or a few slots. No slot will receive keys from more than one partition family because the tag strings (`p:0`, `fp:0`, `b:0`, `q:0`) are distinct.

#### 2.4 Hash Tag Slot Distribution

Valkey Cluster assigns slots via `crc16(hash_tag_content) % 16384`. The hash tag content is the string between `{` and `}`:

| Hash Tag | Content Hashed | Example Slot |
|---|---|---|
| `{p:0}` | `p:0` | `crc16("p:0") % 16384` |
| `{p:1}` | `p:1` | `crc16("p:1") % 16384` |
| `{fp:0}` | `fp:0` | `crc16("fp:0") % 16384` |
| `{b:0}` | `b:0` | `crc16("b:0") % 16384` |
| `{q:0}` | `q:0` | `crc16("q:0") % 16384` |

Since the prefixes differ (`p:`, `fp:`, `b:`, `q:`), same-numbered partitions across families map to different slots. No collisions.

**Slot imbalance risk:** With 256 execution partitions, some slots will receive 0 partitions and some will receive 2+. In practice, CRC-16 distributes well enough that the imbalance is minor. For production, validate the slot distribution at deployment and rebalance Valkey Cluster slots if needed.

#### 2.5 Valkey Cluster Rebalancing vs FlowFabric Repartitioning

These are two distinct operations that operators must not confuse:

**Valkey Cluster rebalancing (adding/removing shards, reshuffling slots): SAFE and TRANSPARENT.** Valkey Cluster moves hash slots between shards. Keys follow their slots automatically. FlowFabric partition assignments do not change — `crc16(hash_tag)` maps to the same slot regardless of which shard serves it. No FlowFabric code changes or data migration needed. Safe to do at any time under normal Valkey Cluster operational procedures.

**FlowFabric repartitioning (changing `num_partitions`): BREAKING CHANGE requiring full migration.** All partition assignments (`crc16(entity_id) % N`) change when N changes. Every stored `partition_id` becomes wrong. Every key name containing `{p:N}` must be re-keyed. Every index entry must move. V1 does not support this. Choose partition counts conservatively at initial deployment.

---

### 3. Cross-Partition Operation Catalog

Every operation that spans more than one partition family. For each: the partition sequence, what happens at each step, failure modes, and convergence mechanism.

#### 3.1 Three-Partition Claim Flow

**Operation:** Worker claims an eligible execution.
**Partitions touched:** `{q:K}` → `{b:M}` → `{p:N}` (→ `{p:N}` again for `acquire_lease`)

```
Worker                Scheduler                {q:K}              {b:M}             {p:N}
  │                      │                       │                  │                 │
  ├─claim_request──────►│                       │                  │                 │
  │                      ├─check_admission─────►│                  │                 │
  │                      │◄──────admitted────────│                  │                 │
  │                      ├─check_budget────────────────────────►│                 │
  │                      │◄──────────────────────────budget_ok──│                 │
  │                      ├─issue_claim_grant──────────────────────────────────────►│
  │                      │◄──────────────────────────────────────────grant_issued──│
  │                      │                       │                  │                 │
  ├─acquire_lease (Lua)───────────────────────────────────────────────────────────►│
  │◄──────────────────────────────────────────────────────────────────lease+attempt─│
```

**Step 1 — Quota admission check (`{q:K}`)**
- Lua: `check_admission_and_record.lua` on the quota partition.
- Sliding-window rate check + concurrency cap check.
- If admitted: ZADD to window, INCR concurrency counter. Proceed.
- If denied: return `rate_limit_exceeded` or `concurrency_limit_exceeded`. Stop.

**Step 2 — Budget check (`{b:M}`)**
- Lua: read `ff:budget:{b:M}:<budget_id>:usage` and `ff:budget:{b:M}:<budget_id>:limits`.
- Advisory read — no mutation. Usage is not pre-charged.
- If any attached budget's hard limit is already breached: return `budget_admission_denied`. Stop.
- If OK: proceed.

**Step 3 — Issue claim-grant (`{p:N}`)**
- Lua: `issue_claim_grant.lua` on the execution's partition.
- Validates execution is still `runnable` + `eligible_now` + `unowned`.
- Writes `ff:exec:{p:N}:<execution_id>:claim_grant` with TTL.
- ZREM from eligible sorted set (prevents double-grant).
- If execution already claimed/cancelled: return error. Stop.

**Step 4 — Acquire lease (`{p:N}`)**
- Lua: `acquire_lease.lua` on the same partition as step 3.
- Validates and consumes the claim grant.
- Creates lease, creates attempt, transitions execution to `active`.
- Updates all partition-local indexes atomically.
- If grant expired or mismatched: return error.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Step 1 rejected (quota) | No side effects. Worker retries or backs off. | Immediate — no cleanup needed. |
| Step 2 rejected (budget) | Step 1 already admitted (INCR'd concurrency). | **Stale INCR.** Concurrency reconciler corrects drift periodically. Acceptable: one extra count until reconciliation. |
| Step 3 fails (execution gone) | Step 1 INCR'd, step 2 passed. | Same as above for step 1. No grant written — no orphan. |
| Step 3 succeeds, step 4 never runs (worker crash) | Grant key exists with TTL. Execution removed from eligible set. | Grant TTL expires (5s). **Grant expiry reconciler** (RFC-009 §12.8) detects execution is `runnable` + `eligible_now` but not in eligible set, re-adds it. |
| Step 3 succeeds, step 4 fails (execution state changed between 3 and 4) | Grant exists but execution no longer claimable. | Grant TTL expires. Reconciler checks execution state — since it's no longer `runnable`, no re-add needed. Grant key auto-deletes. |
| **Quota partition `{q:K}` unreachable** (network partition, shard down) | Scheduler cannot check admission. | **FAIL-OPEN.** Proceed without quota check. Quota is admission pacing, not a correctness boundary. Over-admission is bounded by the claim-grant serialization on `{p:N}`. Reconcile quota counters when `{q:K}` recovers. |
| **Budget partition `{b:M}` unreachable** — hard-limit budget | Scheduler cannot check budget. | **FAIL-CLOSED.** Do not issue claim grant. Budget hard limits are a correctness boundary (spending caps). Execution remains eligible but unclaimed until `{b:M}` recovers. |
| **Budget partition `{b:M}` unreachable** — advisory/soft budget | Scheduler cannot check budget. | **FAIL-OPEN.** Proceed without budget check. Advisory budgets are monitoring tools, not hard enforcement. Log warning. |
| **Execution partition `{p:N}` unreachable** | Cannot access execution state at all. | **FAIL.** Claim is impossible — the execution, lease, and attempt all live on `{p:N}`. Scheduler skips this execution until the partition recovers. |

#### 3.2 Flow Dependency Resolution

**Operation:** Add a dependency edge between two flow member executions, then later resolve it when the upstream completes.

**Sub-operation A: Add dependency edge**
**Partitions touched:** `{fp:N}` → `{p:N}` (downstream child)

```
Control Plane         {fp:N} (flow)            {p:N} (child exec)
  │                      │                         │
  ├─stage_dependency───►│                         │
  │◄──grant_issued──────│                         │
  ├─apply_to_child────────────────────────────────►│
  │◄──────────────────────────────────applied──────│
  ├─finalize_edge──────►│                         │
  │◄──edge_active───────│                         │
```

1. **Stage edge on `{fp:N}`**: Validates membership, checks for cycles (DAG mode), creates edge with `edge_state = pending`, increments `graph_revision`, creates mutation grant.
2. **Apply to child on `{p:N}`**: Consumes grant. Creates `ff:exec:{p:N}:<exec_id>:dep:<edge_id>`. Adds to unresolved set. Increments `unsatisfied_required_count`. If execution is `runnable`, sets `eligibility_state = blocked_by_dependencies`, `blocking_reason = waiting_for_children`, `public_state = waiting_children`.
3. **Finalize edge on `{fp:N}`**: Sets `edge_state = active`.

**Sub-operation B: Resolve dependency**
**Partitions touched:** `{fp:N}` (read outgoing edges) → `{p:N}` (each downstream child)

```
Engine (on upstream     {fp:N} (flow)            {p:N} (child exec)
 terminal event)           │                         │
  ├─read outgoing────────►│                         │
  │◄──edge_ids────────────│                         │
  ├─resolve_dependency (per child)────────────────►│
  │◄──────────────────────────────satisfied/skip───│
```

1. **Read outgoing edges on `{fp:N}`**: `SMEMBERS ff:flow:{fp:N}:<flow_id>:out:<upstream_id>`. Get edge_ids.
2. **For each edge, resolve on child's `{p:N}`**: `resolve_dependency.lua`. If upstream succeeded: mark edge `satisfied`, decrement unresolved count, if all resolved → set `eligible_now` + add to eligible sorted set. If upstream failed: mark edge `impossible`, increment impossible count → set execution to terminal `skipped`.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Step 1 (stage) succeeds, step 2 (apply) fails | Edge staged but not applied. Child has no gating record. | The mutation grant has a TTL. Control plane retries step 2 using the same grant (idempotent — `apply_dependency_to_child` checks `EXISTS` before writing). |
| Step 2 succeeds, step 3 (finalize) fails | Edge applied to child but still `pending` on flow. Child is correctly gated. | Control plane retries step 3. `graph_revision` ensures stale finalizations are rejected. |
| Resolve: flow read succeeds, child resolution fails for one child | Some children resolved, some not. | Engine retries failed child resolutions. `resolve_dependency` is idempotent — already-resolved edges return `already_resolved`. |

#### 3.3 Budget Enforcement on Usage Report

**Operation:** Worker reports usage during active execution. Budget must be checked.
**Partitions touched:** `{p:N}` (attempt usage) → `{b:M}` (each attached budget)

```
Worker                  {p:N} (execution)        {b:M} (budget)
  │                         │                        │
  ├─report_usage──────────►│                        │
  │                         ├─HINCRBY attempt usage  │
  │                         ├─HINCRBY exec usage     │
  │                         │  (if execution-scoped  │
  │                         │   budget on {p:N}:     │
  │                         │   atomic breach check) │
  │                         ├─report_to_budget──────►│
  │                         │◄──breach_status────────│
  │◄──usage_ack + breach────│                        │
```

**For execution-scoped budgets (colocated on `{p:N}`):** Usage increment + breach check is atomic in one Lua script. Class A.

**For flow/lane/tenant-scoped budgets (on `{b:M}`):** After the `{p:N}` attempt usage update, the control plane issues a separate `report_usage_and_check.lua` call to each `{b:M}` partition. If a hard breach is detected, the control plane applies the enforcement action (fail/suspend) back on `{p:N}` in a separate call.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| `{p:N}` attempt usage succeeds, `{b:M}` budget call fails | Attempt usage recorded. Budget counter stale (under-counted). | Next `report_usage` retries the budget increment. Budget is under-counted — not over-counted — so no false breach. Convergent: eventual usage catches up. |
| `{b:M}` reports hard breach, enforcement call to `{p:N}` fails | Budget knows it's breached but execution continues. | Next `report_usage` re-checks budget and re-applies enforcement. Budget is checked at every enforcement point (§1.7 of RFC-008). |
| Concurrent workers both pass budget check before either's increment registers | Budget over-consumed by one concurrent request. | Accepted trade-off for cross-partition budgets. Budget is rechecked on next `report_usage`. Over-consumption is bounded by one concurrent request per check interval. |

#### 3.4 Budget Enforcement on Child Spawn

**Operation:** Flow coordinator creates a child execution; flow-scoped budget must be checked.
**Partitions touched:** `{b:M}` (budget check) → `{fp:N}` (flow membership) → `{p:N}` (child execution creation)

1. **Check budget on `{b:M}`**: Read usage and limits. If `on_hard_limit = deny_child` and budget exhausted, reject.
2. **Add membership on `{fp:N}`**: `add_execution_to_flow`.
3. **Create execution on child's `{p:N}`**: `create_execution` with `flow_id` and `parent_execution_id`.

**Failure modes:** If step 1 passes but another concurrent spawn exhausts the budget before step 3 completes, the child is created but the budget is over-consumed by one request. Convergent via next enforcement point.

#### 3.5 Async Quota Concurrency Decrement

**Operation:** Lease released on `{p:N}` → concurrency counter decremented on `{q:K}`.
**Partitions touched:** `{p:N}` → `{q:K}`

The lease-release Lua scripts (complete/fail/suspend/revoke/reclaim on `{p:N}`) cannot atomically DECR a counter on `{q:K}`. Instead:

1. **Lease-release script on `{p:N}`** completes and returns.
2. **Caller issues async DECR** to `ff:quota:{q:K}:<policy_id>:concurrency`.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| DECR call fails or is lost | Counter drifts high — claims are over-rejected (safe direction). | **Concurrency reconciler** (RFC-008 §4.5): periodically counts actual active leases for the scope, resets counter if it diverges. |
| DECR succeeds but INCR from claim was lost | Counter drifts low — one extra claim may be admitted. | Reconciler corrects. Bounded: one extra concurrent execution per lost INCR. |
| Valkey crash between INCR and DECR | Same as above — directional drift. | Reconciler corrects on next run. |

**Reconciler frequency:** Every 30–60 seconds per quota scope. Walks the authoritative `{p:N}` partition-local worker lease sets and execution core records to compute actual active count.

#### 3.6 Flow Summary Projection

**Operation:** Execution state changes on `{p:N}` → flow summary updated on `{fp:N}`.
**Partitions touched:** `{p:N}` → `{fp:N}`

Flow summary counters (members_active, members_completed, aggregate_tokens, etc.) on `ff:flow:{fp:N}:<flow_id>:summary` are derived projections. They cannot be atomically updated with execution state transitions because the flow and execution live on different partitions.

**Approach:** Event-driven projection.

1. Execution state transition completes on `{p:N}`.
2. If execution has a `flow_id`, the control plane publishes a lightweight state-change event.
3. A **flow summary projector** consumes these events and issues HINCRBY/HSET updates to the summary hash on `{fp:N}`.

**Failure modes:** Summary may lag behind member execution truth by seconds. This is acceptable — the summary is a read-optimization, not an authoritative source. Flow completion decisions that depend on member states must re-read member execution records, not rely solely on the summary.

#### 3.7 Cross-Partition Secondary Index Updates

**Operation:** After atomic transitions on `{p:N}`, update secondary indexes that have no hash tag.
**Keys affected:** `ff:ns:<namespace>:executions`, `ff:tag:<namespace>:<key>:<value>`, `ff:idem:<namespace>:<idempotency_key>`.

**Approach:** Best-effort write after the atomic script returns. These writes are not transactional with the primary transition. On failure, the secondary index becomes stale. A background reconciler can rebuild these indexes from the authoritative partition data.

**Staleness impact:** A `list_executions` query using the namespace index may briefly miss a just-created execution or include a just-deleted one. Queries that require authoritative data should use partition-local indexes.

#### 3.8 Add Execution to Flow (Two-Phase Ownership Claim)

**Operation:** Add an execution to a flow's membership. Enforces single-flow invariant (F2) cross-partition.
**Partitions touched:** (optional `{b:M}` budget check) → `{p:N}` (execution) → `{fp:N}` (flow)

1. **Budget check on `{b:M}`** (if flow has `deny_child` budget): read budget usage. If exhausted, reject.
2. **Phase 1 — ownership claim on `{p:N}`**: Lua script atomically checks `flow_id` field on exec core. If non-empty → `execution_already_in_flow`. If empty → SET `flow_id = <flow_id>`. This is the serialization point that prevents two flows from claiming the same execution.
3. **Phase 2 — membership on `{fp:N}`**: SADD to flow membership set. Increment `graph_revision`. Apply flow policy defaults. Idempotent — if this step fails and is retried, SADD on an existing member is a no-op.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Phase 1 succeeds, phase 2 fails | Execution has `flow_id` set but is not in the flow's membership set. | Retry phase 2. SADD is idempotent. Flow summary projector will eventually detect the member when it reads exec core. |
| Two concurrent adds to different flows | Both reach phase 1 on the same `{p:N}` — serialized. First wins, second gets `execution_already_in_flow`. | Immediate — {p:N} serialization prevents the race. |

#### 3.9 Flow Cancellation Propagation

**Operation:** Flow is cancelled — propagate to member executions.
**Partitions touched:** `{fp:N}` (flow members) → `{p:N}` (each member execution)

```
Operator / Engine       {fp:N} (flow)            {p:N} (member execs)
  │                        │                         │
  ├─cancel_flow──────────►│                         │
  │                        │ 1. Read cancellation    │
  │                        │    propagation policy   │
  │                        │ 2. SMEMBERS members     │
  │                        │    set                  │
  │◄──member_list──────────│                         │
  │                        │                         │
  ├─for each member (policy-filtered):──────────────►│
  │  cancel_execution.lua  │                         │
  │  (per member's {p:N})  │                         │
  │◄─────────────────────────────────────────ok──────│
  │                        │                         │
  ├─update flow state─────►│                         │
  │  (set public_flow_     │                         │
  │   state = cancelled)   │                         │
  │◄──────────────────────│                         │
```

**Cancellation policies (RFC-007):**

| Policy | Member filter | Active execution handling |
|---|---|---|
| `cancel_all` | All members. | Cancel active executions (bypasses lease unconditionally — `cancel_flow` is implicitly operator-privileged). |
| `cancel_unscheduled_only` | Members not yet active (`runnable`, `delayed`, `suspended`). | Active members allowed to finish. |
| `let_active_finish` | Block unclaimed runnable members (`blocked_by_operator`). | Active members finish. When all drain, cancel blocked. |
| `coordinator_decides` | Coordinator execution determines scope. | Per-branch decision. |

**Per-member cancellation:** Each `cancel_execution` runs on the member's `{p:N}` partition. The operation is idempotent — cancelling an already-terminal execution is a no-op.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Flow members read succeeds, some per-member cancellations fail | Partial propagation. Some members remain non-cancelled. | Engine retries failed cancellations. `cancel_execution` is idempotent — already-cancelled members return success. |
| Flow state update fails after members are cancelled | Members cancelled but flow still shows `running`. | Flow summary projector detects all members terminal and updates `public_flow_state` on next cycle. |
| `let_active_finish`: active members crash after blocking unclaimed | Unclaimed members stuck in `blocked_by_operator`. Active members eventually reclaimed or timed out. | When no active members remain (detected by flow summary projector), cancel the blocked members. |

---

### 4. Background Scanners and Reconcilers

Every periodic background process required by the architecture, consolidated in one place.

| Scanner | Target Key | Frequency | Action | RFC |
|---|---|---|---|---|
| **Delayed → Eligible promotion** | `ff:idx:{p:N}:lane:<lane_id>:delayed` | 0.5–1s per partition | `ZRANGEBYSCORE -inf <now>`. Verify execution state, set `eligible_now`, ZADD eligible set with priority score. | 001, 009 |
| **Lease expiry / reclaim** | `ff:idx:{p:N}:lease_expiry` | 1–2s per partition | `ZRANGEBYSCORE -inf <now>`. Run `mark_lease_expired_if_due.lua` or `reclaim_execution.lua`. Re-validates execution core; no-op if lease was renewed. | 003 |
| **Suspension timeout** | `ff:idx:{p:N}:suspension_timeout` | 2–5s per partition | `ZRANGEBYSCORE -inf <now>`. Run `expire_suspension.lua`. Applies timeout behavior (fail/cancel/expire/auto_resume/escalate). | 004 |
| **Pending waitpoint expiry** | `ff:idx:{p:N}:pending_waitpoint_expiry` | 5–10s per partition | `ZRANGEBYSCORE -inf <now>`. Close waitpoints with `close_reason = never_committed`. | 004 |
| **Claim-grant expiry reconciler** | Execution core records | 5–10s per partition | Find executions where `lp = runnable` AND `es = eligible_now` but not in any eligible sorted set. Re-add if no grant key exists. | 009 |
| **Budget counter reconciler** | `ff:budget:{b:M}:<budget_id>:usage` | 30–60s per budget partition | Sum actual attempt-level usage across attached executions. Compare against budget usage hash. Correct if diverged. | 008 |
| **Quota concurrency reconciler** | `ff:quota:{q:K}:<policy_id>:concurrency` | 10–30s per quota scope | Count actual active leases for the scope (walk `{p:N}` partition worker-lease sets). Reset counter if drifted. | 008 |
| **Budget reset** | `ff:idx:{b:M}:budget_resets` | 10–30s per budget partition | `ZRANGEBYSCORE -inf <now>`. Run `reset_budget.lua`. Zero usage, set `next_reset_at`, re-score in index. | 008 |
| **Flow summary projector** | `ff:flow:{fp:N}:<flow_id>:summary` | Event-driven + 10–30s catchup | Consume execution state-change events. Update member counts and aggregate usage. Derive `public_flow_state`. | 007 |
| **Lease history trimmer** | `ff:exec:{p:N}:<execution_id>:lease:history` | Inline (`MAXLEN ~` on XADD) + 60–120s background | Inline: on every lease history append. Background: scan for streams exceeding size threshold and apply aggressive trim. | 003 |
| **Stream time-based cleanup** | `ff:stream:{p:N}:*:meta` | 60s per partition | Check `closed_at + retention_ttl_ms < now`. Delete stream + meta keys. | 006 |
| **Terminal retention** | `ff:idx:{p:N}:lane:<lane_id>:terminal` | Minutes–hours | `ZRANGEBYSCORE -inf <retention_cutoff>`. Purge execution + all sub-keys (cascade delete). | 001, 002, 005, 006 |
| **Worker heartbeat expiry** | `ff:worker:<worker_instance_id>` | Handled by TTL | Valkey auto-expires worker registration keys when heartbeat TTL lapses. Scheduler skips expired workers. | 009 |
| **Dependency resolution reconciler** | `ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies` | 10–30s per partition | For each blocked execution: read upstream terminal states (cross-partition). If upstream is terminal, run `resolve_dependency.lua`. Safety net for engine crash between parent complete and child resolve dispatch. | 007, 010 |

---

## Part 2 — Lua Script Inventory and Operation Flows

## 4. Lua Script Inventory

Every Lua script defined across RFC-001 through RFC-009. All scripts within a single invocation touch only keys sharing one hash tag (`{p:N}`, `{b:M}`, `{q:K}`, or `{fp:N}`).

### 4.1 Execution Partition Scripts (`{p:N}`)

| # | Script Name | RFC | Purpose | Class | Key Count | KEYS (all `{p:N}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 0 | `create_execution` | RFC-001 | Create execution core hash + payload + policy + tags, set initial state vector (all 7 dims), ZADD to eligible or delayed or blocked set, SET NX idempotency key. If timeout_policy.deadline_at set: ZADD execution_deadline index. | A | 7 | exec_core, payload, policy, tags, eligible_or_delayed_zset, idem_key, execution_deadline_zset |
| 1 | `claim_execution` | RFC-001 | Consume claim-grant, create new attempt + lease, transition runnable→active. ZADD attempt_timeout index (score=now+timeout_ms). | A | 14 | exec_core, claim_grant, eligible_zset, lease_expiry_zset, worker_leases, attempt_hash, attempt_usage, attempt_policy, attempts_zset, lease_current, lease_history, active_index, attempt_timeout_zset, execution_deadline_zset |
| 2 | `claim_resumed_execution` | RFC-001 | Consume claim-grant, resume existing attempt (interrupted→started), new lease, transition runnable→active. ZADD attempt_timeout (score=now+remaining_timeout). | A | 11 | exec_core, claim_grant, eligible_zset, lease_expiry_zset, worker_leases, existing_attempt_hash, lease_current, lease_history, active_index, attempt_timeout_zset, execution_deadline_zset |
| 3 | `complete_execution` | RFC-001 | Validate lease, end attempt, close stream, transition active→terminal(success). ZREM from attempt_timeout + execution_deadline. | A | 12 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, lease_current, lease_history, active_index, stream_meta, result_key, attempt_timeout_zset, execution_deadline_zset |
| 4 | `fail_execution` | RFC-001 | Validate lease, end attempt, close stream, decide retry. ZREM attempt_timeout. Retry: ZADD new timeout. Terminal: ZREM execution_deadline. | A | 16 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, delayed_zset, lease_current, lease_history, new_attempt_hash, new_attempt_usage, new_attempt_policy, attempts_zset, active_index, stream_meta, attempt_timeout_zset, execution_deadline_zset |
| 5 | `acquire_lease` | RFC-003 | *See #1 (`claim_execution`). Same logical operation described from lease perspective.* | A | — | *See #1* |
| 6 | `renew_lease` | RFC-003 | Validate lease identity + epoch + expiry, extend expires_at, update lease_expiry index | A | 4 | exec_core, lease_current, lease_history, lease_expiry_zset |
| 7 | `complete_or_fail` | RFC-003 | *See #3/#4. Generic template described from lease perspective.* | A | — | *See #3/#4* |
| 8 | `revoke_lease` | RFC-003 | Validate target lease, set ownership_state=lease_revoked, record revocation, update indexes | A | 5 | exec_core, lease_current, lease_history, lease_expiry_zset, worker_leases |
| 9 | `reclaim_execution` | RFC-003 | Validate expired/revoked ownership, consume claim-grant, create new attempt + lease, increment epoch | A | 15 | exec_core, old_attempt, new_attempt, attempts_zset, new_policy, new_usage, old_lease, new_lease, lease_history, lease_expiry_zset, old_worker_leases, new_worker_leases, claim_grant, old_stream_meta, active_index |
| 10 | `create_and_start_attempt` | RFC-002 | *See #1 (`claim_execution`). Same logical operation described from attempt perspective.* | A | — | *See #1* |
| 11 | `end_attempt_and_decide` | RFC-002 | *See #3/#4. Same logical operation described from attempt perspective. Includes stream close.* | A | — | *See #3/#4* |
| 12 | `interrupt_and_reclaim` | RFC-002 | *See #9 (`reclaim_execution`). Same logical operation described from attempt perspective. Includes stream close.* | A | — | *See #9* |
| 12a | `cancel_execution` | RFC-001 | Cancel from any non-terminal state. Active: validate lease or operator override, end attempt, close stream, clear lease. Runnable: ZREM from eligible/delayed/blocked. Suspended: close waitpoint+suspension. All paths: set terminal_outcome=cancelled. **ZADD terminal_zset is UNCONDITIONAL** — even if execution is not in any sorted set (handles grant-cancel race where grant ZREM'd from eligible but claim never completed). | A | 12 | exec_core, attempt_hash, stream_meta, lease_current, lease_history, lease_expiry_zset, worker_leases, suspension_current, waitpoint_hash, suspension_timeout_zset, terminal_zset, source_state_zset (eligible or delayed or blocked or suspended or active_index) |
| 12b | `replay_execution` | RFC-001 | Validate terminal, create new replay attempt, transition terminal→runnable, ZREM terminal, ZADD eligible | A | 8 | exec_core, new_attempt_hash, new_attempt_usage, new_attempt_policy, attempts_zset, terminal_zset, eligible_zset, lease_history |
| 13 | `suspend_execution` | RFC-004 | Validate lease, release ownership, create suspension + waitpoint (or activate pending), ZADD suspended_zset, update execution state, update indexes | A | 13 | exec_core, attempt_record, lease_current, lease_history, lease_expiry_zset, worker_leases, suspension_current, waitpoint_hash, waitpoint_signals, suspension_timeout_zset, pending_wp_expiry_zset, active_index, suspended_zset |
| 14 | `resume_execution` | RFC-004 | Validate suspension + waitpoint satisfied, close suspension + waitpoint, ZREM suspended_zset, transition suspended→runnable, update indexes | A | 8 | exec_core, suspension_current, waitpoint_hash, waitpoint_signals, suspension_timeout_zset, eligible_zset, delayed_zset, suspended_zset |
| 15 | `create_pending_waitpoint` | RFC-004 | Validate active lease, create pending waitpoint with short expiry, add to pending_wp_expiry index | A | 3 | exec_core, waitpoint_hash, pending_wp_expiry_zset |
| 16 | `expire_suspension` | RFC-004 | Validate suspension still active + timeout due, apply timeout_behavior (fail/cancel/expire/auto_resume/escalate), close suspension + waitpoint | A | 7 | exec_core, suspension_current, waitpoint_hash, suspension_timeout_zset, terminal_zset or eligible_zset, lease_history |
| 17 | `deliver_signal` | RFC-005 | Validate target, check idempotency, record signal, evaluate resume condition, optionally close waitpoint + suspension + transition suspended→runnable | A | 13 | exec_core, wp_condition, wp_signals_stream, exec_signals_zset, signal_hash, signal_payload, idem_key, waitpoint_hash, suspension_current, eligible_zset, suspended_zset, delayed_zset, suspension_timeout_zset |
| 18 | `buffer_signal_for_pending_waitpoint` | RFC-005 | Accept signal for pending waitpoint without evaluating resume conditions | A | 7 | exec_core, wp_condition, wp_signals_stream, exec_signals_zset, signal_hash, signal_payload, idem_key |
| 19 | `timeout_waitpoint` | RFC-005 | Generate synthetic timeout signal, apply timeout_behavior, close waitpoint + suspension, transition execution state | A | 8 | exec_core, wp_condition, wp_signals_stream, signal_hash, waitpoint_hash, suspension_current, suspension_timeout_zset, target_state_zset |
| 20 | `append_frame` | RFC-006 | Validate lease (lease_id + epoch + expiry), lazy-create stream metadata, XADD frame, XTRIM retention | B | 3 | exec_core, stream_key, stream_meta |
| 21 | `close_stream` | RFC-006 | Set closed_at + closed_reason on stream metadata (called within end_attempt scripts) | A | 1 | stream_meta |
| 22 | `apply_dependency_to_child` | RFC-007 | Create dep record, increment unsatisfied count. If runnable: set blocked_by_dependencies, ZREM eligible, ZADD blocked:dependencies | A | 6 | exec_core, deps_meta, unresolved_set, dep_hash, eligible_zset, blocked_deps_zset |
| 23 | `resolve_dependency` | RFC-007 | Satisfy or skip. Satisfaction: ZREM blocked:deps, set eligible_now, ZADD eligible. Skip: ZREM blocked:deps, set terminal=skipped, ZADD terminal | A | 7 | exec_core, deps_meta, unresolved_set, dep_hash, eligible_zset, terminal_zset, blocked_deps_zset |
| 24 | `evaluate_flow_eligibility` | RFC-007 | Read-only check of execution + dependency state, return eligibility status | C | 2 | exec_core, deps_meta |
| 25 | `issue_claim_grant` | RFC-009 | Validate execution eligible, check no existing grant, write grant with TTL, remove from eligible set | A | 3 | exec_core, claim_grant_key, eligible_zset |
| 26 | `issue_reclaim_grant` | RFC-009 | Validate ownership_state in {expired_reclaimable, revoked}, write grant with TTL. No ZREM (exec not in eligible set) | A | 2 | exec_core, claim_grant_key |
| 27 | `promote_delayed` | RFC-001/009 | Verify runnable + not_eligible_until_time, set eligible_now + waiting_for_worker + waiting, ZREM delayed, ZADD eligible with composite score | A | 3 | exec_core, delayed_zset, eligible_zset |
| 28 | `mark_lease_expired_if_due` | RFC-003 | Re-validate lease expiry on exec core, set ownership_state=lease_expired_reclaimable if confirmed. No-op if renewed/reclaimed | A | 4 | exec_core, lease_current, lease_expiry_zset, lease_history |
| 29a | `block_execution_for_admission` | RFC-009 | Parameterized block: set eligibility/blocking/public for budget/quota/route/lane denial, ZREM eligible, ZADD target blocked set. All 7 dims. | A | 3 | exec_core, eligible_zset, target_blocked_zset |
| 29b | `unblock_execution` | RFC-010 | Re-evaluate blocked execution, set eligible_now + waiting_for_worker, ZREM blocked set, ZADD eligible with composite priority score. All 7 dims. | A | 3 | exec_core, source_blocked_zset, eligible_zset |
| 29c | `expire_execution` | RFC-001 | Engine-initiated on attempt timeout or execution deadline. Validate active + running. Set terminal_outcome=expired, all 7 dims. Clear lease, close stream, ZREM from lease_expiry + worker_leases + active_index + attempt_timeout + execution_deadline. ZADD terminal. | A | 12 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, lease_current, lease_history, active_index, stream_meta, attempt_timeout_zset, execution_deadline_zset, suspended_zset |

### 4.2 Flow Partition Scripts (`{fp:N}`)

| # | Script Name | RFC | Purpose | Class | Key Count | KEYS (all `{fp:N}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 29 | `stage_dependency_edge` | RFC-007 | Validate membership + topology, check `expected_graph_revision` matches current (reject with `stale_graph_revision` on mismatch), create edge record, increment graph_revision, create mutation grant. ARGV must include `expected_graph_revision`. | A | 6 | flow_core, members_set, edge_hash, out_adj_set, in_adj_set, grant_hash |

### 4.3 Budget Partition Scripts (`{b:M}`)

| # | Script Name | RFC | Purpose | Class | Key Count | KEYS (all `{b:M}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 30 | `report_usage_and_check` | RFC-008 | Atomically HINCRBY usage per dimension, check hard/soft limits, return breach status | A | 3 | budget_usage, budget_limits, budget_def |
| 31 | `reset_budget` | RFC-008 | Zero all usage counters, record reset event, compute next_reset_at, re-score in reset index | A | 3 | budget_usage, budget_def, budget_resets_zset |

### 4.4 Quota Partition Scripts (`{q:K}`)

| # | Script Name | RFC | Purpose | Class | Key Count | KEYS (all `{q:K}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 32 | `check_admission_and_record` | RFC-008 | Sliding-window ZREMRANGEBYSCORE + ZCARD rate check, INCR concurrency check, ZADD + INCR on admit | A | 3 | window_zset, concurrency_counter, quota_def |

### 4.5 Script Overlap Notes

**Overlap group A:** Scripts 1/5/10 (`claim_execution` / `acquire_lease` / `create_and_start_attempt`) are the same operation from RFC-001/003/002 perspectives. Implement as ONE script. RFC-001 #1 is canonical. #5 and #10 are listed as cross-references only.

**Overlap group B:** Scripts 3+4/7/11 (`complete_execution` + `fail_execution` / `complete_or_fail` / `end_attempt_and_decide`). RFC-003 #7 and RFC-002 #11 are generic templates; RFC-001 #3 and #4 are the concrete implementations. Implement #3 and #4 as two scripts (or one parameterized script). #7 and #11 are cross-references.

**Overlap group C:** Scripts 9/12 (`reclaim_execution` / `interrupt_and_reclaim`). Same operation. RFC-002 #12 includes stream close detail. Implement as ONE script.

**Overlap group D:** Scripts 16/19 (`expire_suspension` / `timeout_waitpoint`). Same operation. RFC-005 #19 has the detailed timeout_behavior mapping. Implement as ONE script.

Script 21 (`close_stream`) is NOT a standalone script — it is inline logic within `end_attempt_and_decide` (#11) and `interrupt_and_reclaim` (#12). Listed for completeness but not independently invocable.

**Cancel safety note:** Script 12a (`cancel_execution`) MUST ZADD to `terminal_zset` unconditionally on all paths, regardless of which source set the execution was in. During the grant-cancel race window (a claim-grant was issued, ZREM'd from eligible, but `acquire_lease` hasn't run yet), the execution may not be in any scheduling index. If cancel runs during this window and skips the terminal ZADD, the execution becomes a phantom — not in any index, not discoverable. ZADD to terminal_zset is the safety net.

### 4.6 Claim Dispatch Logic

The scheduler must dispatch to the correct claim script based on execution state:

| Execution State | Attempt State | Script | New Attempt? |
|----------------|---------------|--------|-------------|
| `runnable` + `eligible_now` | `pending_first_attempt`, `pending_retry_attempt`, or `pending_replay_attempt` | `claim_execution` (#1) | Yes |
| `runnable` + `eligible_now` | `attempt_interrupted` (resumed after suspension) | `claim_resumed_execution` (#2) | No (continues same attempt) |
| `active` + `lease_expired_reclaimable` or `lease_revoked` | any | `reclaim_execution` (#9) via `issue_reclaim_grant` (#26) | Yes (new attempt) |

**Note:** RFC-002's `attempt_type = fallback` creates a new attempt with `attempt_state = pending_retry_attempt` (fallback is a form of retry at a different tier). No separate `pending_fallback_attempt` state exists.

### 4.7 Summary

| Partition | Script Count | Notes |
|-----------|-------------|-------|
| `{p:N}` execution | 30 | All claim/lease/attempt/suspension/signal/stream/promotion/cancel/replay operations. |
| `{fp:N}` flow | 1 | Topology validation + edge staging. |
| `{b:M}` budget | 2 | Usage increment + breach check, reset. |
| `{q:K}` quota | 1 | Admission check + record. |
| **Total listed** | **34** | Including cross-reference entries for overlap groups. |
| **Unique for implementation** | **~25** | After deduplicating overlap groups A (claim: -2), B (complete/fail: -2), C (reclaim: -1), D (timeout: -1), inline close_stream (-1), plus cancel+replay added (+2). |

### 4.8 Implementation Notes for Lua Scripts

These notes apply to ALL Lua scripts across all RFCs. Mandatory reading before implementation.

#### (a) HGETALL returns a flat array — use a conversion helper

Valkey `HGETALL` returns a flat array `{key1, val1, key2, val2, ...}`, NOT a Lua table with named fields. All RFC Lua pseudocode uses `core.lifecycle_phase` syntax which assumes a dict-style table. Every script must begin with a conversion:

```lua
local function hgetall_to_table(flat)
  local t = {}
  for i = 1, #flat, 2 do
    t[flat[i]] = flat[i + 1]
  end
  return t
end

local core = hgetall_to_table(redis.call("HGETALL", KEYS[1]))
-- Now core.lifecycle_phase works
```

Alternatively, use `HMGET` to fetch only the specific fields needed — more efficient for large hashes. The execution core has 50+ fields but most scripts check 5-10.

#### (b) Lua scripts are NOT transactional — partial writes persist on abort

Valkey Lua scripts execute each `redis.call()` independently. If a script aborts mid-execution (OOM, bug, or `redis.call()` error propagation), **all writes that already executed within the script persist**. There is no rollback.

Example: if `HSET exec_core lifecycle_phase=terminal` succeeds but the subsequent `ZADD terminal_zset` fails (OOM), the execution is terminal but not indexed. The generalized index reconciler (§6.17) catches this, but implementers must understand the risk.

**Defensive write ordering:** Where possible, perform index writes (ZADD, ZREM, SADD) BEFORE state mutations (HSET exec_core). If an index write OOMs, the state hasn't changed — the execution remains in its previous consistent state. This isn't always possible (some HSET fields are needed before index writes), but the principle is: delay the "point of no return" state mutation as late as possible in the script.

#### (b2) All exec_core mutations MUST go through Lua scripts

No raw `HSET` calls from application code. Every state transition is an atomic Lua script that validates preconditions, updates the state vector, and maintains indexes in one call. A raw `HSET` that skips validation can violate invariants (e.g., setting `lifecycle_phase = active` without creating a lease).

#### (c) Empty string means "cleared", not nil

Lua scripts use `""` (empty string) to represent cleared optional fields, not `nil`. Valkey hashes cannot store nil — `HSET key field ""` stores an empty string. Check cleared fields with `field == ""` or `field == nil` (nil if the field was never set). Both mean "no value."

#### (d) Priority score is IEEE 754 double — safe up to ~priority 9000

The composite score `-(priority * 1_000_000_000_000) + created_at_ms` is stored as a Valkey sorted set score (IEEE 754 double, 53-bit mantissa). With `created_at_ms` around 1.7 × 10^12 (current epoch ms), the score magnitude is `priority * 10^12 + 1.7 * 10^12`. IEEE 754 doubles lose integer precision above 2^53 = 9.0 × 10^15. Safe priority range: `|priority| <= ~9000` before FIFO tiebreaking becomes unreliable. V1 recommended range `[-1000, 1000]` is safe with wide margin.

#### (e) Dependency resolution trigger: engine dispatches after complete

`complete_execution` does NOT call `resolve_dependency` inline — they may be on different partitions. Instead, `complete_execution` returns a flag or event indicating the execution has a `flow_id`. The engine layer (outside Lua) then:
1. Reads outgoing edges from `{fp:N}` flow partition.
2. For each downstream child, calls `resolve_dependency.lua` on the child's `{p:N}` partition.

This cross-partition dispatch is documented in §3.2 (Flow Dependency Resolution). If the engine crashes between `complete_execution` returning and the dependency resolution dispatch, the dependency resolution reconciler (§6) catches the gap.

#### (f) Flow completion policy MUST read authoritative member states

The flow summary hash (`ff:flow:{fp:N}:<flow_id>:summary`) is eventually consistent (§3.6). Do NOT use it as the sole input for completion/failure policy decisions. When evaluating whether a flow is `completed`, `failed`, or `cancelled`, the implementation MUST re-read authoritative execution core records from each member's `{p:N}` partition. The summary may lag by seconds, and premature flow completion would incorrectly cancel still-active members.

Use the summary for: dashboards, monitoring, approximate counts, fast inspection.
Do NOT use the summary for: completion policy evaluation, failure policy decisions, cancellation propagation scope.

#### (g) Lease renewal history events are OPTIONAL

The `renew_lease` script's XADD to lease_history is a performance-sensitive hot path. At 1M active executions with 10s renewal intervals, renewal events alone generate 100K XADD/s system-wide. Each entry is ~100 bytes and trimmed by MAXLEN, but the append overhead on every heartbeat is avoidable.

The execution core field `lease_last_renewed_at` already provides the latest renewal timestamp without stream overhead. The important lease history events — `acquired`, `released`, `expired`, `revoked`, `reclaimed` — are far less frequent and MUST always be recorded.

**V1 default:** Renewal history events OFF. The `renew_lease` script skips the XADD for `"event", "renewed"`. Enable per-lane when detailed ownership audit trails are needed for debugging.

#### (h) Worker current_claim_count — async INCR/DECR pattern

The `current_claim_count` field on `ff:worker:<worker_instance_id>` (global key) tracks how many active leases a worker holds. The capability matcher reads this for `max_concurrent_claims` enforcement. But lease acquisition on `{p:N}` cannot atomically update a global key.

**Maintenance pattern** (same as quota concurrency counter, §6.6):
- After `claim_execution` / `claim_resumed_execution` succeeds on `{p:N}`: caller issues async `HINCRBY ff:worker:<wid> current_claim_count 1`.
- After lease release (complete/fail/suspend/revoke/reclaim) on `{p:N}`: caller issues async `HINCRBY ff:worker:<wid> current_claim_count -1`.
- **Worker claim count reconciler** (§6.18): periodically sums `SCARD ff:idx:{p:N}:worker:<wid>:leases` across all execution partitions. If the sum diverges from `current_claim_count`, resets the count.

Without this pattern, `current_claim_count` stays at 0 and `max_concurrent_claims` is never enforced.

### 4.9 Lua Script Return Convention

All Lua scripts use `err()` and `ok()` in pseudocode. Concrete return format:

**Success:** `return {1, "OK", ...values}`
**Failure:** `return {0, "ERROR_NAME", ...context}`

Client SDK parses element 1: `1` = success, `0` = failure.

**Do NOT use `redis.error_reply()` for Class A scripts** — it prevents returning structured context. Class B scripts (e.g., `append_frame`) MAY use `redis.error_reply()` for simplicity per RFC-006 convention.

| Pseudocode | Concrete Return |
|---|---|
| `return err("stale_lease")` | `return {0, "stale_lease"}` |
| `return err("budget_exceeded")` | `return {0, "budget_exceeded", dimension, usage, limit}` |
| `return err("rate_limit_exceeded")` | `return {0, "rate_limit_exceeded", retry_after_ms}` |
| `return ok()` | `return {1, "OK"}` |
| `return ok(lease_id, epoch, expires_at)` | `return {1, "OK", lease_id, epoch, expires_at}` |
| `return ok("retry_scheduled", id, idx, delay)` | `return {1, "OK", "retry_scheduled", id, idx, delay}` |
| `return ok_duplicate(signal_id)` | `return {1, "DUPLICATE", signal_id}` |

**Helper (prepend to every script):**

```lua
local function ok(...)  return {1, "OK", ...} end
local function err(...) return {0, ...} end
```

### 4.10 Usage Counter Consistency Model

Usage counters exist at three levels with different consistency guarantees:

| Level | Key | Updated By | Consistency |
|---|---|---|---|
| **Attempt** | `ff:attempt:{p:N}:...:usage` | `report_usage` HINCRBY on `{p:N}` | **Strongly consistent** — atomic within execution partition. |
| **Execution** | `ff:exec:{p:N}:...:core` (total_*_tokens, etc.) | Same `report_usage` script, same `{p:N}` | **Strongly consistent** — atomic with attempt usage. |
| **Budget** | `ff:budget:{b:M}:...:usage` | Separate `report_usage_and_check` on `{b:M}` | **Eventually consistent** — cross-partition, may lag by one concurrent request. |

**Authoritative sources:** Attempt hash for per-attempt attribution. Execution core for execution-level totals. Budget hash for enforcement (may slightly over/under-count vs sum of attempts).

**Reconciliation:** Budget counter reconciler (§6.5) periodically re-sums attempt-level usage and corrects drift.

---

## 5. Operation Flow Diagrams

ASCII sequence diagrams for the 11 critical execution paths. Each shows which Lua scripts fire, which partitions are touched, and in what order.

### 5a. Submit Execution (create → eligible set)

```
Client                  Engine                   {p:N} Partition
  │                       │                          │
  │  enqueue(lane,        │                          │
  │   payload, opts)      │                          │
  │──────────────────────►│                          │
  │                       │                          │
  │                       │  1. Resolve partition:    │
  │                       │     p = hash(exec_id) %  │
  │                       │     num_partitions       │
  │                       │                          │
  │                       │  2. Check idempotency    │
  │                       │     SET NX ff:idem:...   │
  │                       │  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─►│
  │                       │                          │
  │                       │  3. HSET exec_core       │
  │                       │     lifecycle_phase =    │
  │                       │       runnable           │
  │                       │     eligibility_state =  │
  │                       │       eligible_now       │
  │                       │     attempt_state =      │
  │                       │       pending_first_     │
  │                       │       attempt            │
  │                       │     public_state =       │
  │                       │       waiting            │
  │                       │  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─►│
  │                       │                          │
  │                       │  4. SET payload + policy │
  │                       │  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─►│
  │                       │                          │
  │                       │  5. ZADD eligible set    │
  │                       │     score = -(pri*1T)    │
  │                       │       + created_at       │
  │                       │  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─►│
  │                       │                          │
  │◄──────────────────────│  return execution_id     │
  │                       │                          │

  Lua scripts: none (atomic MULTI/EXEC or single pipeline)
  Partitions: {p:N} only
  If delayed: ZADD delayed set instead of eligible set,
    eligibility_state = not_eligible_until_time
```

### 5b. Claim Execution (scheduler → grant → acquire_lease → attempt start)

```
Worker          Scheduler           {q:K}         {b:M}         {p:N}
  │                │                  │             │              │
  │ claim_request  │                  │             │              │
  │ (caps, lanes)  │                  │             │              │
  │───────────────►│                  │             │              │
  │                │                  │             │              │
  │                │ 1. QUOTA CHECK   │             │              │
  │                │ check_admission_ │             │              │
  │                │ and_record.lua   │             │              │
  │                │─────────────────►│             │              │
  │                │ (ADMITTED/DENIED)│             │              │
  │                │◄─────────────────│             │              │
  │                │                  │             │              │
  │                │ 2. BUDGET CHECK  │             │              │
  │                │ (advisory read   │             │              │
  │                │  of usage vs     │             │              │
  │                │  limits — no     │             │              │
  │                │  mutation)       │             │              │
  │                │────────────────────────────────►│              │
  │                │                  │  (OK/BREACH)│              │
  │                │◄───────────────────────────────│              │
  │                │                  │             │              │
  │                │ 3. SELECT CANDIDATE            │              │
  │                │ (fairness, priority,           │              │
  │                │  capability match)             │              │
  │                │                  │             │              │
  │                │ 4. ISSUE GRANT   │             │              │
  │                │ issue_claim_     │             │              │
  │                │ grant.lua        │             │              │
  │                │──────────────────────────────────────────────►│
  │                │                  │             │  (grant key  │
  │                │                  │             │   written,   │
  │                │                  │             │   exec ZREM  │
  │                │◄─────────────────────────────────────────────│
  │                │                  │             │              │
  │ grant_info     │                  │             │              │
  │◄───────────────│                  │             │              │
  │                │                  │             │              │
  │ 5. ACQUIRE LEASE                  │             │              │
  │ claim_execution.lua               │             │              │
  │ (or claim_resumed_execution.lua   │             │              │
  │  if attempt_state = interrupted)  │             │              │
  │────────────────────────────────────────────────────────────────►
  │                                   │             │  (validate   │
  │                                   │             │   grant,     │
  │                                   │             │   create     │
  │                                   │             │   attempt+   │
  │                                   │             │   lease,     │
  │                                   │             │   HSET core, │
  │                                   │             │   ZADD       │
  │                                   │             │   expiry,    │
  │                                   │             │   XADD       │
  │◄───────────────────────────────────────────────────────────────│
  │ (lease_id, epoch,                 │             │              │
  │  attempt_id, exec data)           │             │              │

  Lua scripts: check_admission_and_record → budget read (advisory) →
               issue_claim_grant → claim_execution
  Partitions: {q:K} → {b:M} (read) → {p:N} → {p:N}
              (4 sequential calls, 3 potentially different shards)
```

### 5c. Complete Execution (lease validate → attempt end → stream close → indexes)

```
Worker                                {p:N} Partition
  │                                       │
  │ complete_execution                    │
  │ (exec_id, lease_id, epoch,            │
  │  attempt_id, result)                  │
  │──────────────────────────────────────►│
  │                                       │
  │  complete_execution.lua               │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: lifecycle=active,     │
  │  │    lease_id + epoch + attempt_id   │
  │  │    match, lease not expired        │
  │  │ 3. HSET attempt: ended_success    │
  │  │ 4. HSET stream_meta: closed_at    │
  │  │    (if stream exists)              │
  │  │ 5. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=terminal              │
  │  │    ownership=unowned               │
  │  │    terminal_outcome=success        │
  │  │    attempt_state=attempt_terminal  │
  │  │    public_state=completed          │
  │  │    clear lease fields              │
  │  │ 6. ZREM lease_expiry              │
  │  │ 7. SREM worker_leases             │
  │  │ 8. ZADD terminal_zset             │
  │  │ 9. DEL lease_current              │
  │  │10. XADD lease_history             │
  │  └────────────────────────────────────┤
  │                                       │
  │◄──────────────────────────────────────│
  │  ok()                                 │
  │                                       │
  │  (async) Engine decrements quota      │
  │  concurrency counter on {q:K}         │

  Lua scripts: complete_execution (1 script)
  Partitions: {p:N} only (+ async {q:K} decrement)
```

### 5d. Fail + Retry (lease validate → attempt end → new attempt → delayed)

```
Worker                                {p:N} Partition
  │                                       │
  │ fail_execution                        │
  │ (exec_id, lease_id, epoch,            │
  │  attempt_id, reason, category)        │
  │──────────────────────────────────────►│
  │                                       │
  │  fail_execution.lua                   │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate lease                  │
  │  │ 3. HSET attempt: ended_failure    │
  │  │    (stays ended_failure — no       │
  │  │     superseded overwrite)          │
  │  │ 4. Check retry_count < max_retries │
  │  │                                    │
  │  │ [RETRY PATH]                       │
  │  │ 5. Create new attempt hash:        │
  │  │    type=retry, state=created       │
  │  │    previous_attempt_index set      │
  │  │ 6. ZADD attempts_zset             │
  │  │ 7. HSET new attempt_policy        │
  │  │ 8. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=runnable              │
  │  │    eligibility=not_eligible_       │
  │  │      until_time                    │
  │  │    attempt_state=pending_retry     │
  │  │    public_state=delayed            │
  │  │    retry_count++                   │
  │  │ 9. ZREM lease_expiry              │
  │  │10. SREM worker_leases             │
  │  │11. ZADD delayed_zset (backoff)    │
  │  │12. DEL lease_current              │
  │  │13. XADD lease_history             │
  │  │                                    │
  │  │ [TERMINAL PATH — if max retries]   │
  │  │ 5. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=terminal              │
  │  │    terminal_outcome=failed         │
  │  │    public_state=failed             │
  │  │ 6. ZADD terminal_zset             │
  │  │ 7. DEL lease, clear indexes       │
  │  └────────────────────────────────────┤
  │                                       │
  │◄──────────────────────────────────────│
  │  ok(retry_scheduled | terminal_failed)│

  Lua scripts: fail_execution (1 script, branching)
  Partitions: {p:N} only (+ async {q:K} decrement)
  Later: promotion scanner moves from delayed→eligible
         when backoff expires
```

### 5e. Suspend (lease validate → release → suspension + waitpoint create)

```
Worker                                {p:N} Partition
  │                                       │
  │ (optional) create_pending_            │
  │ waitpoint.lua                         │
  │ (before calling external tool)        │
  │──────────────────────────────────────►│
  │  writes pending waitpoint, returns    │
  │  waitpoint_key to give to ext system  │
  │◄──────────────────────────────────────│
  │                                       │
  │  ... external tool call ...           │
  │                                       │
  │ suspend_execution                     │
  │ (exec_id, attempt_idx, attempt_id,    │
  │  lease_id, epoch, suspension_spec)    │
  │──────────────────────────────────────►│
  │                                       │
  │  suspend_execution.lua                │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: active + lease match  │
  │  │ 3. Check no active suspension      │
  │  │ 4. If pending waitpoint:           │
  │  │    activate it (pending→active)    │
  │  │    ZREM pending_wp_expiry          │
  │  │    Else: create new waitpoint      │
  │  │ 5. Create suspension record        │
  │  │    (reason, condition, timeout,     │
  │  │     continuation pointer)          │
  │  │ 6. Update attempt: interrupted     │
  │  │ 7. DEL lease_current              │
  │  │ 8. ZREM lease_expiry              │
  │  │ 9. SREM worker_leases             │
  │  │10. ZREM active_index              │
  │  │11. XADD lease_history (released)  │
  │  │12. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=suspended             │
  │  │    ownership=unowned               │
  │  │    attempt_state=attempt_          │
  │  │      interrupted                   │
  │  │    public_state=suspended          │
  │  │    set current_suspension_id       │
  │  │    set current_waitpoint_id        │
  │  │13. If timeout: ZADD               │
  │  │    suspension_timeout_zset         │
  │  └────────────────────────────────────┤
  │                                       │
  │◄──────────────────────────────────────│
  │  ok(suspension_id, wp_id, wp_key)     │

  Lua scripts: [create_pending_waitpoint] + suspend_execution
  Partitions: {p:N} only
```

### 5f. Signal → Resume (signal deliver → condition evaluate → resume → eligible)

```
External System                       {p:N} Partition
  │                                       │
  │ send_signal                           │
  │ (waitpoint_key, signal_name,          │
  │  payload, idempotency_key)            │
  │──────────────────────────────────────►│
  │                                       │
  │  (Engine decodes waitpoint_key        │
  │   to extract partition + wp_id)       │
  │                                       │
  │  deliver_signal.lua                   │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: suspended or has      │
  │  │    pending waitpoint               │
  │  │ 3. HGETALL wp_condition            │
  │  │    validate: not closed            │
  │  │ 4. Check idempotency (SET NX)      │
  │  │ 5. HSET signal_hash (record)       │
  │  │ 6. SET signal_payload              │
  │  │ 7. XADD wp_signals_stream         │
  │  │ 8. ZADD exec_signals_zset         │
  │  │ 9. Evaluate resume condition:      │
  │  │    match signal against matchers   │
  │  │    update satisfied_count          │
  │  │                                    │
  │  │ [IF CONDITION SATISFIED]           │
  │  │10. HSET wp_condition: closed=1     │
  │  │11. HSET waitpoint: state=closed    │
  │  │12. HSET suspension: closed,        │
  │  │    close_reason=resumed            │
  │  │13. ZREM suspension_timeout_zset    │
  │  │14. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=runnable              │
  │  │    ownership=unowned               │
  │  │    terminal_outcome=none           │
  │  │    attempt_state=attempt_          │
  │  │      interrupted                   │
  │  │    public_state=waiting (or        │
  │  │      delayed if resume_delay)      │
  │  │    clear suspension_id + wp_id     │
  │  │15. ZREM suspended_zset             │
  │  │16. ZADD eligible_zset (or          │
  │  │    delayed_zset if delay)          │
  │  │17. HSET signal: observed_effect=   │
  │  │    resume_condition_satisfied      │
  │  │                                    │
  │  │ [IF NOT SATISFIED]                 │
  │  │10. HSET signal: observed_effect=   │
  │  │    appended_to_waitpoint           │
  │  └────────────────────────────────────┤
  │                                       │
  │◄──────────────────────────────────────│
  │  ok(signal_id, observed_effect)       │
  │                                       │
  │  (If resumed: worker claims via       │
  │   normal 5b flow using                │
  │   claim_resumed_execution.lua)        │

  Lua scripts: deliver_signal (1 script)
  Partitions: {p:N} only
  No cross-partition calls. Waitpoint key is self-routing.
```

### 5g. Reclaim (scanner detect → interrupt attempt → new attempt + lease)

```
Engine Scanner          Scheduler           {p:N} Partition
  │                       │                     │
  │ 1. ZRANGEBYSCORE      │                     │
  │    lease_expiry       │                     │
  │    -inf <now>         │                     │
  │    LIMIT 0 batch      │                     │
  │─────────────────────────────────────────────►│
  │ (returns expired      │                     │
  │  execution_ids)       │                     │
  │◄────────────────────────────────────────────│
  │                       │                     │
  │ 2. For each expired:  │                     │
  │    mark_lease_expired │                     │
  │    (optional — sets   │                     │
  │     ownership_state=  │                     │
  │     lease_expired_    │                     │
  │     reclaimable)      │                     │
  │─────────────────────────────────────────────►│
  │◄────────────────────────────────────────────│
  │                       │                     │
  │ 3. Notify scheduler   │                     │
  │    "exec X needs      │                     │
  │     reclaim"          │                     │
  │──────────────────────►│                     │
  │                       │                     │
  │                       │ 4. Find capable     │
  │                       │    worker (caps,     │
  │                       │    fairness)         │
  │                       │                     │
  │                       │ 5. Issue reclaim     │
  │                       │    grant             │
  │                       │    (issue_reclaim_   │
  │                       │     grant.lua —      │
  │                       │     validates        │
  │                       │     ownership_state  │
  │                       │     is reclaimable,  │
  │                       │     no ZREM needed)  │
  │                       │─────────────────────►│
  │                       │◄────────────────────│
  │                       │                     │
Worker (new)              │                     │
  │                       │                     │
  │ 6. RECLAIM            │                     │
  │ reclaim_execution.lua │                     │
  │ (or interrupt_and_    │                     │
  │  reclaim.lua)         │                     │
  │─────────────────────────────────────────────►│
  │  ┌──────────────────────────────────────────┤
  │  │ a. Validate: ownership_state in          │
  │  │    {expired_reclaimable, revoked}        │
  │  │ b. Validate claim-grant                  │
  │  │ c. Set old attempt: interrupted_         │
  │  │    reclaimed, ended_at                   │
  │  │ d. Close old attempt stream              │
  │  │ e. DEL old lease, SREM old worker_leases │
  │  │ f. XADD lease_history (expired+reclaimed)│
  │  │ g. Create new attempt: type=reclaim,     │
  │  │    state=started                         │
  │  │ h. ZADD attempts_zset                    │
  │  │ i. New lease: incremented epoch          │
  │  │ j. HSET exec_core: ALL 7 dims           │
  │  │    lifecycle=active, ownership=leased    │
  │  │    attempt_state=running_attempt         │
  │  │    reclaim_count++                       │
  │  │ k. ZADD lease_expiry (new)               │
  │  │ l. SADD new worker_leases                │
  │  │ m. DEL claim_grant                       │
  │  └──────────────────────────────────────────┤
  │                                             │
  │◄────────────────────────────────────────────│
  │  ok(new_attempt_id, new_epoch, exec_data)   │

  Lua scripts: mark_lease_expired_if_due (optional) +
               issue_reclaim_grant + reclaim_execution
  Partitions: {p:N} only (scanner + reclaim on same partition)
```

### 5h. Flow Child Spawn + Dependency Resolution

```
Coordinator Exec    Engine        {fp:N} Flow     {p:N} Child      {p:N} Parent
  │                   │           Partition        Partition        Partition
  │                   │               │                │               │
  │ 1. create_child_  │               │                │               │
  │    execution       │               │                │               │
  │ (flow_id,         │               │                │               │
  │  parent_id,       │               │                │               │
  │  payload)         │               │                │               │
  │──────────────────►│               │                │               │
  │                   │               │                │               │
  │                   │ 2. Create child execution      │               │
  │                   │    (same as 5a Submit)         │               │
  │                   │    BUT: eligibility=           │               │
  │                   │    blocked_by_dependencies     │               │
  │                   │──────────────────────────────►│               │
  │                   │                │               │               │
  │                   │ 3. Add to flow membership      │               │
  │                   │    stage_dependency_edge.lua   │               │
  │                   │────────────────►│               │               │
  │                   │  (validate      │               │               │
  │                   │   members,      │               │               │
  │                   │   cycle check,  │               │               │
  │                   │   create edge,  │               │               │
  │                   │   issue grant)  │               │               │
  │                   │◄───────────────│               │               │
  │                   │                │               │               │
  │                   │ 4. Apply dependency to child   │               │
  │                   │    apply_dependency_to_        │               │
  │                   │    child.lua                   │               │
  │                   │──────────────────────────────►│               │
  │                   │  (create dep    │              │               │
  │                   │   record,       │              │               │
  │                   │   increment     │              │               │
  │                   │   unsatisfied)  │              │               │
  │                   │◄─────────────────────────────│               │
  │                   │                │              │               │
  │◄──────────────────│               │              │               │
  │  ok(child_exec_id)│               │              │               │
  │                   │               │              │               │
  ═══════════════════ LATER: parent completes ═══════════════════════
  │                   │               │              │               │
  │                   │ 5. Parent execution completes │               │
  │                   │    (complete_execution.lua)   │               │
  │                   │───────────────────────────────────────────────►│
  │                   │◄──────────────────────────────────────────────│
  │                   │               │              │               │
  │                   │ 6. Engine detects parent      │               │
  │                   │    terminal, reads outgoing   │               │
  │                   │    edges from flow partition  │               │
  │                   │────────────────►│              │               │
  │                   │ (out-adj set)  │              │               │
  │                   │◄───────────────│              │               │
  │                   │               │              │               │
  │                   │ 7. For each downstream edge:  │               │
  │                   │    resolve_dependency.lua     │               │
  │                   │    on child partition         │               │
  │                   │──────────────────────────────►│               │
  │                   │  ┌───────────────────────────┤               │
  │                   │  │ a. HSET dep: satisfied    │               │
  │                   │  │ b. SREM unresolved        │               │
  │                   │  │ c. HINCRBY unsatisfied -1 │               │
  │                   │  │ d. If remaining==0:        │               │
  │                   │  │    HSET exec_core:         │               │
  │                   │  │    eligibility=eligible_now│               │
  │                   │  │    ZADD eligible_zset      │               │
  │                   │  │    (composite priority     │               │
  │                   │  │     score)                 │               │
  │                   │  │ e. If upstream failed:     │               │
  │                   │  │    HSET exec_core:         │               │
  │                   │  │    lifecycle=terminal       │               │
  │                   │  │    terminal_outcome=skipped│               │
  │                   │  └───────────────────────────┤               │
  │                   │◄─────────────────────────────│               │
  │                   │               │              │               │

  Lua scripts: (submit) + stage_dependency_edge + apply_dependency_to_child
               ... later: complete_execution + resolve_dependency
  Partitions: {p:N} child + {fp:N} flow + {p:N} parent
              (3 partitions, sequential calls)
  Cross-partition: flow→child dependency application uses
                   idempotent mutation grant pattern
```

### 5i. Cancel Execution

```
Operator/Owner                        {p:N} Partition
  │                                       │
  │ cancel_execution                      │
  │ (exec_id, reason, lease_id?,          │
  │  lease_epoch?, operator_override?)    │
  │──────────────────────────────────────►│
  │                                       │
  │  cancel_execution.lua                 │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 1a. If terminal: return           │
  │  │     execution_already_terminal    │
  │  │ 2. If active: validate lease      │
  │  │    (or operator override)          │
  │  │    End attempt: ended_cancelled    │
  │  │    Close stream if exists          │
  │  │    DEL lease, clear indexes        │
  │  │ 3. If runnable: ZREM from         │
  │  │    eligible/delayed/blocked set    │
  │  │ 4. If suspended: close waitpoint  │
  │  │    + suspension, ZREM timeout      │
  │  │ 5. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=terminal              │
  │  │    terminal_outcome=cancelled      │
  │  │    public_state=cancelled          │
  │  │ 6. ZADD terminal_zset             │
  │  │    (UNCONDITIONAL — handles       │
  │  │     grant-cancel race where exec  │
  │  │     was ZREM'd from eligible by   │
  │  │     grant but claim never ran)    │
  │  └────────────────────────────────────┤
  │◄──────────────────────────────────────│

  Lua scripts: cancel_execution (1 script, multi-source-state)
  Partitions: {p:N} only (+ async {q:K} decrement if was active)
```

### 5j. Replay Execution (terminal → new attempt)

```
Operator                              {p:N} Partition
  │                                       │
  │ replay_execution                      │
  │ (exec_id, replay_reason,              │
  │  requested_by)                        │
  │──────────────────────────────────────►│
  │                                       │
  │  replay_execution.lua                 │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: lifecycle=terminal    │
  │  │ 3. Create new attempt hash:        │
  │  │    type=replay, state=created      │
  │  │    replay_reason, requested_by     │
  │  │    replayed_from_attempt_index     │
  │  │ 4. ZADD attempts_zset             │
  │  │ 5. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=runnable              │
  │  │    terminal_outcome=none           │
  │  │    attempt_state=pending_replay    │
  │  │    public_state=waiting            │
  │  │    replay_count++                  │
  │  │ 6. ZREM terminal_zset             │
  │  │ 7. ZADD eligible_zset (composite  │
  │  │    priority score)                 │
  │  └────────────────────────────────────┤
  │◄──────────────────────────────────────│
  │  ok(new_attempt_id, attempt_index)    │

  Lua scripts: replay_execution (1 script)
  Partitions: {p:N} only
```

### 5k. Renew Lease

```
Worker                                {p:N} Partition
  │                                       │
  │ renew_lease                           │
  │ (exec_id, lease_id, epoch,            │
  │  attempt_id)                          │
  │──────────────────────────────────────►│
  │                                       │
  │  renew_lease.lua                      │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: active, not revoked, │
  │  │    not expired, lease_id + epoch   │
  │  │    + attempt_id all match          │
  │  │ 3. HSET exec_core: new            │
  │  │    lease_expires_at,               │
  │  │    lease_last_renewed_at,          │
  │  │    lease_renewal_deadline          │
  │  │ 4. HSET lease_current: same       │
  │  │ 5. ZADD lease_expiry (new score)  │
  │  │ 6. XADD lease_history (renewed)   │
  │  └────────────────────────────────────┤
  │◄──────────────────────────────────────│
  │  ok(new_expires_at)                   │

  Lua scripts: renew_lease (1 script)
  Partitions: {p:N} only
```

### 5.12 Partition Touch Summary

| Operation | {p:N} | {fp:N} | {b:M} | {q:K} | Total Partitions |
|-----------|-------|--------|-------|-------|-----------------|
| Submit execution | 1 | — | — | — | 1 |
| Claim execution | 1 | — | 1 | 1 | 3 |
| Claim resumed execution | 1 | — | 1 | 1 | 3 |
| Complete execution | 1 | — | — | 1* | 1+1* |
| Fail + retry | 1 | — | — | 1* | 1+1* |
| Cancel execution | 1 | — | — | 1* | 1+1* |
| Suspend | 1 | — | — | — | 1 |
| Signal → resume | 1 | — | — | — | 1 |
| Reclaim | 1 | — | — | — | 1 |
| Replay | 1 | — | — | — | 1 |
| Renew lease | 1 | — | — | — | 1 |
| Flow child + deps | 2† | 1 | — | — | 3 |

`*` = async concurrency decrement, not in critical path
`†` = child partition + parent partition (may be same or different)


## Part 3 — Background Processes, Capacity, and Deployment


## 6. Background Process Catalog

FlowFabric requires several background processes to maintain system correctness, enforce time-based policies, and reconcile cross-partition state. Every process listed here is **idempotent** — running it twice produces the same result as running it once. This is critical because background scanners may crash and restart, overlap with concurrent instances, or process the same item multiple times.

### 6.1 Lease Expiry Scanner

**Source:** RFC-003 (Lease and Fencing Semantics)

**Purpose:** Detects expired leases and marks executions as reclaimable so stale workers are fenced out and work can be recovered.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 1-2 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | `ff:idx:{p:N}:lease_expiry` (ZSET, score = `lease_expires_at` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:lease_expiry -inf <now_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Run `mark_lease_expired_if_due.lua` on the execution core. The script re-validates that the lease is actually expired (guards against clock skew or recent renewal). If confirmed, sets `ownership_state = lease_expired_reclaimable`. Optionally runs `reclaim_execution.lua` immediately or defers to the scheduler. |
| **Failure mode** | Safe to crash. On restart, re-scans from `-inf`. Items already processed are idempotent (script re-validates). |
| **Idempotency** | The Lua script checks current lease state before acting. If already expired/reclaimed, it's a no-op. |
| **Batch size** | 50-100 per scan cycle. Larger batches risk Lua script latency spikes. |

### 6.2 Suspension Timeout Scanner

**Source:** RFC-004 (Suspension and Waitpoint Model)

**Purpose:** Applies timeout behavior (fail, cancel, expire, auto-resume, escalate) when suspended executions exceed their `timeout_at` deadline.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 2-5 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | `ff:idx:{p:N}:suspension_timeout` (ZSET, score = `timeout_at` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:suspension_timeout -inf <now_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Run `expire_suspension.lua`. The script re-validates that the execution is still suspended and the timeout is still due (guards against concurrent resume). Applies the configured `timeout_behavior` (fail/cancel/expire/auto_resume/escalate) atomically. Sets all 7 state vector dimensions. |
| **Failure mode** | Safe to crash. Un-processed timeouts remain in the index and are picked up on next scan. |
| **Idempotency** | The script checks `lifecycle_phase == suspended` and `timeout_at <= now`. If already resumed/cancelled/timed-out, it's a no-op. |
| **Batch size** | 20-50 per cycle. Timeout processing may involve multiple state transitions (close waitpoint, close suspension, update execution). |

### 6.3 Pending Waitpoint Expiry Scanner

**Source:** RFC-004 (Suspension and Waitpoint Model)

**Purpose:** Cleans up pending waitpoints that were pre-created but never committed to a suspension (the worker completed synchronously or crashed before suspending).

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 5-10 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | `ff:idx:{p:N}:pending_waitpoint_expiry` (ZSET, score = `expires_at` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:pending_waitpoint_expiry -inf <now_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Check waitpoint state. If still `pending`, set state to `expired`, set `close_reason = never_committed`. Remove from expiry index. Record buffered signals as `no_op_waitpoint_never_activated`. |
| **Failure mode** | Safe to crash. Pending waitpoints have short TTL (typically 30-60s). Worst case: a stale pending waitpoint accepts a few extra signals before cleanup. |
| **Idempotency** | Checks `state == pending` before acting. Already-expired or already-activated waitpoints are skipped. |
| **Batch size** | 50-100. Lightweight operations. |

### 6.4 Claim-Grant Expiry Reconciler

**Source:** RFC-009 (Scheduling, Fairness, and Admission)

**Purpose:** Recovers executions that were removed from the eligible sorted set when a claim-grant was issued (RFC-009 §12.7 step 5) but the grant was never consumed (expired without `acquire_lease` being called).

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 5-10 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | No dedicated index. Scans execution core records where `lifecycle_phase = runnable` AND `eligibility_state = eligible_now` but the execution is not in the partition's eligible sorted set. |
| **Operation** | For each partition, compare execution core states against sorted set membership. Alternatively, scan `ff:exec:{p:N}:<execution_id>:claim_grant` keys and check for expired TTL. |
| **Per-item action** | If grant key is missing (expired) and execution is still `runnable` + `eligible_now` + `unowned`, re-add to `ff:idx:{p:N}:lane:<lane_id>:eligible` with correct priority score. |
| **Failure mode** | Safe to crash. Executions stuck outside the eligible set are detected on next scan. Worst case: a few seconds of invisible-but-eligible executions. |
| **Idempotency** | Checks execution state before re-adding. Already-claimed or already-in-set executions are skipped. ZADD on an existing member is a no-op if score matches. |
| **Batch size** | 100-200. Lightweight ZADD operations. |

**Implementation note:** This reconciler is a safety net. Under normal operation, grants are consumed within milliseconds. The reconciler catches edge cases: scheduler crash after grant issuance, network partition between scheduler and worker, worker crash before calling `acquire_lease`.

### 6.5 Budget Counter Reconciler

**Source:** RFC-008 (Budget, Quota, and Rate-Limit Policy)

**Purpose:** Corrects drift on cross-partition budget usage counters for **lifetime (non-resetting) budgets only**. Because cross-partition budget increments are not atomic with attempt-level usage updates, the budget counter may drift by small amounts (typically 0-1 concurrent requests worth of overshoot).

**Critical: Budgets with `reset_policy` are SKIPPED.** Resetting budgets (hourly, daily, monthly) cannot be reconciled by summing all-time attempt usage — the sum includes usage from before the last reset, which would undo the reset. Resetting budgets rely solely on the per-`report_usage` atomic HINCRBY path for accuracy. Drift on resetting budgets is bounded to one concurrent request per enforcement point and self-corrects on each subsequent `report_usage` call.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 30-60 seconds per budget partition |
| **Partition scope** | Per `{b:M}` budget partition |
| **Index** | `ff:budget:{b:M}:{budget_id}:executions` (SET of execution_ids attached to this budget) |
| **Operation** | For each budget: **skip if `reset_policy` is set** (non-null). For lifetime budgets only: sum actual attempt-level usage across all attached executions. Compare against budget usage hash. Correct upward if diverged. |
| **Per-item action** | Read `ff:attempt:{p:N}:{execution_id}:{attempt_index}:usage` for each active execution. Sum dimensions. Compare with `ff:budget:{b:M}:{budget_id}:usage`. **Only correct UPWARD** — for each dimension, set counter to `max(current_counter, computed_sum)`. Never correct downward. Upward-only is conservative (blocks sooner) and safe. |
| **Failure mode** | Safe to crash. Drift is bounded (one concurrent request per enforcement point). Reconciliation on next cycle. |
| **Idempotency** | Reads actual state and applies max(). Running twice is safe — max is idempotent. |
| **Batch size** | 1 budget per cycle (budget may have many executions). Limit to 100 execution reads per budget per cycle to avoid cross-partition fan-out. |

**Note:** This reconciler is advisory for v1. The primary enforcement path (per-`report_usage` breach check) is the correctness boundary. The reconciler catches accumulated drift on lifetime budgets over time.

### 6.6 Quota Concurrency Reconciler

**Source:** RFC-008 (Budget, Quota, and Rate-Limit Policy)

**Purpose:** Corrects the concurrency counter (`ff:quota:{q:K}:{quota_policy_id}:concurrency`) which may drift because INCR (on lease acquire) and DECR (on lease release) happen on different partitions and are not atomic with each other.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 10-30 seconds per quota partition |
| **Partition scope** | Per `{q:K}` quota partition |
| **Index** | Quota policy definition lists the scope (lane, tenant). Active leases are on execution partitions. |
| **Operation** | For each quota policy with a concurrency cap: count actual active leases in the scope (scan execution partitions or use cached lane counts from RFC-009). Compare with the concurrency counter. If diverged, reset counter to actual count. |
| **Per-item action** | `SET ff:quota:{q:K}:{quota_policy_id}:concurrency <actual_count>` |
| **Failure mode** | Safe to crash. Counter drift is temporary and bounded. Worst case: a few extra or fewer claims than the cap allows until next reconciliation. |
| **Idempotency** | Overwrites counter with authoritative count. Running twice is safe. |
| **Batch size** | 1 quota policy per cycle. |

### 6.7 Flow Summary Projector

**Source:** RFC-007 (Flow Container and DAG Semantics)

**Purpose:** Maintains the derived flow summary (member counts by state, aggregate usage, unresolved dependencies, derived `public_flow_state`) on the flow partition. Member executions live on arbitrary execution partitions, so the summary cannot be computed atomically — it is a best-effort projection.

| Property | Value |
|---|---|
| **Trigger** | Event-driven (on execution state change events) with periodic fallback |
| **Frequency** | Event-driven: on each execution terminal/claim/suspend/resume transition that is flow-affiliated. Periodic fallback: every 10-30 seconds per flow partition for consistency. |
| **Partition scope** | Per `{fp:N}` flow partition |
| **Index** | `ff:flow:{fp:N}:<flow_id>:members` (SET of execution_ids) |
| **Operation** | For each member execution: read `public_state` from execution core (cross-partition). Aggregate counts. Update `ff:flow:{fp:N}:<flow_id>:summary`. Derive `public_flow_state` from aggregated counts + completion/failure policy. |
| **Per-item action** | HSET on summary hash with updated counts and derived state. |
| **Failure mode** | Safe to crash. Summary may be stale until next projection cycle. Summary is never authoritative for member claimability — execution-local state is authoritative. |
| **Idempotency** | Reads current state and overwrites summary. Running twice is safe. |
| **Batch size** | 1 flow per cycle. Flows with many members (100+) may span multiple cycles. |

**Implementation recommendation:** Use a Valkey Stream per flow partition (`ff:flow:{fp:N}:events`) as an event bus. When an execution changes state, the engine appends an event. The projector consumes from the stream and updates the summary. The periodic fallback catches missed events.

**Stale member handling:** When the projector reads a member execution's core hash and gets key-not-found (execution was purged by the retention scanner), it must:
1. SREM the stale `execution_id` from `ff:flow:{fp:N}:<flow_id>:members`.
2. DEL `ff:flow:{fp:N}:<flow_id>:member:<execution_id>` (member detail hash).
3. Count the removed member as terminal for summary purposes (it completed and was purged).
4. Log a warning for operator visibility.

This prevents ghost members from accumulating in flow membership sets after their executions are purged.

### 6.8 Stream Retention Trimmer

**Source:** RFC-006 (Stream Model)

**Purpose:** Removes expired stream data (Valkey Stream keys and metadata hashes) for closed streams that have exceeded their retention window.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 60 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | Scans `ff:stream:{p:N}:*:meta` hashes where `closed_at + retention_ttl_ms < now`. |
| **Operation** | For each expired stream: `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}` (the Valkey Stream), `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}:meta` (the metadata hash). |
| **Per-item action** | Two DEL commands per expired stream. |
| **Failure mode** | Safe to crash. Streams that should be trimmed remain until next cycle. They consume memory but do not affect correctness. |
| **Idempotency** | DEL on non-existent keys is a no-op. |
| **Batch size** | 100-200. Lightweight DEL operations. |

**Note:** Per-frame MAXLEN trimming happens inline during `append_frame` (RFC-006 Lua script). This scanner handles whole-stream deletion after the retention window.

### 6.9 Lease History Trimmer

**Source:** RFC-003 (Lease and Fencing Semantics)

**Purpose:** Trims lease history streams (`ff:exec:{p:N}:<execution_id>:lease:history`) to prevent unbounded growth for long-lived executions with many lease operations (frequent renewals, multiple reclaims).

| Property | Value |
|---|---|
| **Trigger** | Inline (MAXLEN on each XADD) + periodic background cleanup |
| **Frequency** | Inline: on every lease history append. Background: every 60-120 seconds per partition for executions with large histories. |
| **Partition scope** | Per `{p:N}` execution partition |
| **Operation** | Inline: `XADD ... MAXLEN ~ <lease_history_maxlen> ...` (already in RFC-003 Lua scripts). Background: scan for streams exceeding a size threshold and apply aggressive trim. |
| **Per-item action** | `XTRIM ff:exec:{p:N}:<execution_id>:lease:history MAXLEN <limit>` |
| **Failure mode** | Safe to crash. Untrimmed history consumes extra memory but does not affect correctness. |
| **Idempotency** | XTRIM is idempotent. |
| **Batch size** | 100-200. Lightweight XTRIM operations. |

### 6.10 Delayed Execution Promoter

**Source:** RFC-001 (Execution Object), RFC-009 (Scheduling)

**Purpose:** Promotes executions from the `delayed` sorted set to the `eligible` sorted set when their `delay_until` timestamp passes. Covers retry backoff delays, explicit delays, and resume-with-delay.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 500ms-1s per partition (latency-sensitive — delayed executions should become eligible promptly) |
| **Partition scope** | Per `{p:N}` execution partition, per lane |
| **Index** | `ff:idx:{p:N}:lane:<lane_id>:delayed` (ZSET, score = `delay_until` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:lane:<lane_id>:delayed -inf <now_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Run `promote_delayed.lua`: verify execution is still `runnable` + `not_eligible_until_time`. Set `eligibility_state = eligible_now`, `blocking_reason = waiting_for_worker`, `public_state = waiting`. ZREM from delayed, ZADD to eligible with priority score. |
| **Failure mode** | Safe to crash. Delayed executions stay in the delayed set until promoted. Worst case: a few seconds of late promotion. |
| **Idempotency** | Script checks current state before promoting. Already-eligible or already-claimed executions are skipped. |
| **Batch size** | 100-200. Lightweight state transitions. |

### 6.11 Budget Reset Scanner

**Source:** RFC-008 (Budget, Quota, and Rate-Limit Policy)

**Purpose:** Resets recurring budgets when their reset window expires (e.g., daily cost budget resets at midnight).

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 10-30 seconds (budget resets are typically hourly/daily, so low frequency is fine) |
| **Partition scope** | Per `{b:M}` budget partition |
| **Index** | `ff:idx:{b:M}:budget_resets` (ZSET, score = `next_reset_at` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{b:M}:budget_resets -inf <now_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Run `reset_budget.lua`: zero all usage counters, record reset event, compute `next_reset_at`, re-score in index. |
| **Failure mode** | Safe to crash. Overdue resets are processed on next scan. |
| **Idempotency** | Script checks `next_reset_at` before resetting. If already reset (next_reset_at is in the future), it's a no-op. |
| **Batch size** | 10-20. Budget resets are infrequent. |

### 6.12 Terminal Execution Retention Scanner

**Source:** RFC-001 (Execution Object)

**Purpose:** Purges terminal execution records that have exceeded their retention window. Cascades to all sub-objects (attempts, streams, leases, signals, suspension records).

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 60-300 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition, per lane |
| **Index** | `ff:idx:{p:N}:lane:<lane_id>:terminal` (ZSET, score = `completed_at` ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:lane:<lane_id>:terminal -inf <now_ms - retention_ms> LIMIT 0 <batch_size>` |
| **Per-item action** | Delete all keys for the execution: core hash, payload, result, policy, tags, all attempt hashes/usage/policy, attempt sorted set, all stream data/meta, lease current/history, suspension/waitpoint records, signal records/indexes. ZREM from terminal set, namespace index. |
| **Failure mode** | Safe to crash. Partial deletion is handled by re-scanning (remaining keys are cleaned on next cycle). |
| **Idempotency** | DEL on non-existent keys is a no-op. ZREM on non-existent members is a no-op. |
| **Batch size** | 10-50 (each execution has many sub-keys to delete — cascading deletion is heavier than other scanners). |

### 6.13 Lane Count Aggregator

**Source:** RFC-009 (Scheduling, Fairness, and Admission)

**Purpose:** Maintains cached lane counts (`ff:lane:<lane_id>:counts`) by aggregating ZCARD across partition-local sorted sets. Provides fast dashboard reads without scanning all partitions on every query.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 5-15 seconds per lane |
| **Partition scope** | Cross-partition (reads from all `{p:N}` partitions for a lane) |
| **Index** | Reads `ff:idx:{p:N}:lane:<lane_id>:eligible`, `:delayed`, `:active`, `:suspended`, `:terminal`, plus blocked sets. |
| **Operation** | For each lane: iterate all execution partitions, ZCARD each per-lane sorted set, sum across partitions. Write aggregate counts to `ff:lane:<lane_id>:counts`. |
| **Per-item action** | HSET on the lane counts hash with updated per-state counts. |
| **Failure mode** | Safe to crash. Counts may be stale until next cycle. Not authoritative — ZCARD on individual partition sets is authoritative but slower. |
| **Idempotency** | Overwrites counts with current values. Running twice is safe. |
| **Batch size** | 1 lane per cycle. Each lane requires `num_partitions` ZCARD calls (parallelizable). |

### 6.14 Dependency Resolution Reconciler

**Source:** RFC-007 (Flow Container and DAG Semantics), RFC-010 §3.2

**Purpose:** Safety net for the cross-partition dependency resolution dispatch. When a parent execution completes, the engine dispatches `resolve_dependency` to each downstream child's `{p:N}` partition (§3.2, §4.8e). If the engine crashes between the parent's completion and the child resolution dispatch, downstream children remain stuck in `blocked_by_dependencies` indefinitely. This reconciler detects and resolves that gap.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 10-30 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Index** | `ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies` (ZSET of executions blocked on upstream dependencies) |
| **Operation** | For each execution in the blocked set: read `ff:exec:{p:N}:<execution_id>:deps:meta` to get `flow_id` and unsatisfied count. For each unresolved edge in `ff:exec:{p:N}:<execution_id>:deps:unresolved`: read the upstream execution's terminal state from its `{p:N}` partition (cross-partition read). If upstream is terminal, call `resolve_dependency.lua` on this child's partition. |
| **Per-item action** | `resolve_dependency.lua` per stale unresolved edge. Idempotent — already-resolved edges return `already_resolved`. |
| **Failure mode** | Safe to crash. Unresolved dependencies remain in the blocked set and are picked up on next scan. |
| **Idempotency** | `resolve_dependency` checks edge state before acting. Satisfied or impossible edges are skipped. |
| **Batch size** | 20-50 per cycle. Each item may require 1-3 cross-partition reads (upstream execution states). |

**When this reconciler fires:** Normally, dependency resolution is dispatched by the engine within milliseconds of parent completion. The reconciler is a safety net for: engine crash between parent complete and child resolve dispatch, network partition between engine and Valkey during dispatch, or failure reading outgoing edges from the flow partition.

### 6.15 Eligibility Re-evaluation Scanner (Unblock Scanner)

**Source:** RFC-009 (Scheduling — §11 admission rejection model, §12.9 block script)

**Purpose:** Moves executions from blocked sorted sets back to the eligible sorted set when the blocking condition has cleared. Without this scanner, executions moved to `blocked:budget`, `blocked:quota`, `blocked:route`, or `blocked:operator` by `block_execution_for_admission` would be permanently stuck even after budget is increased, quota window resets, a capable worker registers, or a lane is unpaused.

One unified scanner handles all 4 block types per partition per lane.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 5-10 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition, per lane |
| **Indexes** | `ff:idx:{p:N}:lane:<lane_id>:blocked:budget`, `blocked:quota`, `blocked:route`, `blocked:operator` |
| **Operation** | For each blocked set: ZRANGE LIMIT batch. Per execution: re-evaluate block condition against current resource state. |

**Per-block-type checks:**

| Block Type | Re-evaluation | Cross-Partition Read |
|---|---|---|
| `blocked:budget` | Read attached budget usage + limits on `{b:M}`. Unblock if all budgets have headroom. | Yes (`{b:M}`) |
| `blocked:quota` | Read quota window + concurrency on `{q:K}`. Unblock if admission would succeed. | Yes (`{q:K}`) |
| `blocked:route` | Check worker registrations for capable + active workers with capacity. | Yes (global worker keys) |
| `blocked:operator` (lane-paused) | Read exec core `blocking_reason`. **Only if `paused_by_policy`** (lane block) — skip `paused_by_operator` (explicit hold). Then check lane config; unblock if `state = intake_open`. | Yes (global lane config) |

**Critical: `blocking_reason` field check.** The `blocked:operator` set contains both lane-paused (`paused_by_policy`) and explicit operator-held (`paused_by_operator`) executions. The scanner must read each execution's `blocking_reason` before unblocking. Each block type has an expected reason:

| Scanner | Expected `blocking_reason` | Skip if |
|---|---|---|
| Budget | `waiting_for_budget` | Any other |
| Quota | `waiting_for_quota` | Any other |
| Route | `waiting_for_capable_worker` or `waiting_for_locality_match` | Any other |
| Lane (`blocked:operator`) | `paused_by_policy` | `paused_by_operator` |

| Property | Value |
|---|---|
| **Per-item action** | Read exec core `blocking_reason`. If mismatch with expected: skip. If match: re-evaluate external condition. If cleared: run `unblock_execution.lua` — verify still blocked, set `eligibility_state = eligible_now`, `blocking_reason = waiting_for_worker`, `public_state = waiting`, ZREM blocked set, ZADD eligible set with composite priority score. All 7 dims. |
| **Failure mode** | Safe to crash. Blocked executions remain blocked until next scan. |
| **Idempotency** | `unblock_execution.lua` checks `eligibility_state` before acting. Already-eligible or already-claimed executions are skipped. |
| **Batch size** | 20-50 per blocked set per cycle. |

**Latency note:** 5-10s scan frequency means up to 10s delay before unblocking. For latency-sensitive cases (lane unpause), `enable_lane` may trigger an immediate scan.

**Note on indefinite suspension:** Executions in `lifecycle_phase = suspended` with no `timeout_at` are intentionally permanent until a signal or operator action. This is by design for "wait for human indefinitely" patterns. The unblock scanner does NOT process suspended executions — those are handled by signal delivery (RFC-005) and suspension timeout (§6.2).

### 6.16 Execution/Attempt Timeout Scanner

**Source:** RFC-001 (timeout_policy), RFC-002 (AttemptPolicySnapshot.timeout_ms)

**Purpose:** Enforces per-attempt timeouts and total execution deadlines. Lease expiry (§6.1) detects crashed workers that stop heartbeating. This scanner detects slow-but-alive workers that keep renewing their lease past the execution's allowed processing time.

**Two indexes:**

| Index | Key | Score | Populated By |
|---|---|---|---|
| Per-attempt timeout | `ff:idx:{p:N}:attempt_timeout` | `started_at + timeout_ms` | `claim_execution`, `claim_resumed_execution` |
| Total execution deadline | `ff:idx:{p:N}:execution_deadline` | `deadline_at` from timeout_policy | `create_execution` (if deadline set) |

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 1-2 seconds per partition (latency-sensitive — timeouts should fire promptly) |
| **Partition scope** | Per `{p:N}` execution partition |
| **Operation** | `ZRANGEBYSCORE ff:idx:{p:N}:attempt_timeout -inf <now_ms> LIMIT 0 <batch>`. Same for `execution_deadline`. |
| **Per-item action** | Run `expire_execution.lua`: validate execution is still `active` + lease is still valid (not already completed/failed/reclaimed). If confirmed overdue: set `terminal_outcome = expired`, `attempt_state = attempt_terminal`, `failure_reason = "attempt_timeout"` or `"execution_deadline"`. Clear lease, update all indexes. ZREM from attempt_timeout and execution_deadline. All 7 state vector dimensions. |
| **Failure mode** | Safe to crash. Overdue entries remain in the index. Picked up on next scan. |
| **Idempotency** | Script checks `lifecycle_phase == active` before acting. Already-terminal or already-expired executions are no-ops. |
| **Batch size** | 50-100 per cycle. |

**Important distinction from lease expiry:** Lease expiry (§6.1) fires when the worker stops heartbeating (lease TTL passes). Attempt timeout fires when the worker IS heartbeating but has been running too long. Both may fire — the first one to execute wins; the second is a no-op (execution already terminal).

**Index cleanup:** When an execution completes normally (complete_execution, fail_execution), the script must ZREM from both `attempt_timeout` and `execution_deadline` indexes. When a new attempt starts after retry, the old entry is removed and a new one added with the new `started_at + timeout_ms`.

### 6.17 Generalized Index Reconciler

**Source:** All RFCs. Safety net for OOM partial writes, crashes, and any state/index desync.

**Purpose:** Detects and repairs inconsistencies between execution core state and partition-local index membership. **Bidirectional:** adds executions to indexes they should be in AND removes them from indexes they shouldn't be in. Catches OOM partial writes, crashes, and accumulated phantom entries.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 30-60 seconds per partition |
| **Partition scope** | Per `{p:N}` execution partition |
| **Operation** | SCAN execution core hashes. For each, determine correct index from state, ZADD if missing, ZREM from all incorrect indexes. |

**Reconciliation rules (bidirectional):**

| Execution State | Correct Index | Also ZADD | ZREM From All Others |
|---|---|---|---|
| `lifecycle=terminal` | `terminal` | — | eligible, delayed, active, suspended, all blocked |
| `lifecycle=active` | `active` + `lease_expiry` | both | eligible, delayed, suspended, terminal, all blocked |
| `lifecycle=runnable, es=eligible_now` | `eligible` | — | delayed, active, suspended, terminal, all blocked |
| `lifecycle=runnable, es=not_eligible_until_time` | `delayed` | — | eligible, active, suspended, terminal, all blocked |
| `lifecycle=suspended` | `suspended` (+ `suspension_timeout` if set) | both | eligible, delayed, active, terminal, all blocked |
| `lifecycle=runnable, es=blocked_by_*` | `blocked:<reason>` | — | eligible, delayed, active, suspended, terminal, other blocked |

**CRITICAL: Per-item fix MUST be an atomic Lua script.** Between the SCAN discovery and the fix, the execution's state may change (e.g., claimed by a worker). The Lua re-reads exec_core via HMGET and validates state before any writes. Without atomicity, the reconciler could create phantom index entries.

**Pseudocode: `reconcile_execution_index.lua`**

```lua
-- KEYS: exec_core, eligible_zset, delayed_zset, active_zset,
--       suspended_zset, terminal_zset, lease_expiry_zset,
--       blocked_deps, blocked_budget, blocked_quota,
--       blocked_route, blocked_operator, suspension_timeout
-- ARGV: execution_id

local core = redis.call("HMGET", KEYS[1],
  "lifecycle_phase", "ownership_state", "eligibility_state",
  "priority", "created_at", "completed_at",
  "lease_expires_at", "delay_until")

local lp, os, es = core[1], core[2], core[3]
if lp == nil then return ok("purged") end

local eid = ARGV[1]
local all_sched = {KEYS[2], KEYS[3], KEYS[4], KEYS[5], KEYS[6],
                   KEYS[8], KEYS[9], KEYS[10], KEYS[11], KEYS[12]}

local correct_key, score
if lp == "terminal" then
  correct_key = KEYS[6]
  score = tonumber(core[6]) or 0
elseif lp == "active" then
  correct_key = KEYS[4]
  score = tonumber(core[7]) or 0
  redis.call("ZADD", KEYS[7], score, eid)  -- also ensure lease_expiry
elseif lp == "suspended" then
  correct_key = KEYS[5]; score = 0
elseif lp == "runnable" then
  if es == "eligible_now" then
    correct_key = KEYS[2]
    local pri = tonumber(core[4]) or 0
    local created = tonumber(core[5]) or 0
    score = 0 - (pri * 1000000000000) + created
  elseif es == "not_eligible_until_time" then
    correct_key = KEYS[3]; score = tonumber(core[8]) or 0
  elseif es == "blocked_by_dependencies" then correct_key = KEYS[8]; score = 0
  elseif es == "blocked_by_budget" then correct_key = KEYS[9]; score = 0
  elseif es == "blocked_by_quota" then correct_key = KEYS[10]; score = 0
  elseif es == "blocked_by_route" then correct_key = KEYS[11]; score = 0
  elseif es == "blocked_by_operator" then correct_key = KEYS[12]; score = 0
  end
end
if correct_key == nil then return ok("unknown_state") end

-- ZADD to correct index
redis.call("ZADD", correct_key, score, eid)
-- ZREM from all OTHER scheduling indexes (bidirectional cleanup)
for _, idx in ipairs(all_sched) do
  if idx ~= correct_key then redis.call("ZREM", idx, eid) end
end
return ok("reconciled")
```

| Property | Value |
|---|---|
| **Per-item action** | Atomic Lua: HMGET state, ZADD correct, ZREM all others. No state mutations — only index repairs. |
| **Failure mode** | Safe to crash. Desync persists until next scan. |
| **Idempotency** | ZADD on existing member is no-op. ZREM on non-member is no-op. |
| **Batch size** | 100-200 SCAN iterations. Each Lua: ~13 keys, 1 HMGET + 1 ZADD + ~10 ZREM. |
| **Discovery strategy** | `SMEMBERS ff:idx:{p:N}:all_executions` to get all execution IDs in the partition. O(N_executions) — avoids SCAN which is O(N_total_keys_on_node). For each execution_id: read lane_id from exec_core, construct per-lane index keys, call `reconcile_execution_index.lua`. Process in batches of 100-200 per cycle. |

**When this fires:** Under normal operation, finds nothing (ZADDs match, ZREMs find nothing). Exists for:
- OOM partial writes (Lua aborted after state mutation but before index write)
- Engine crash during multi-step operations
- Accumulated phantom entries from any source
- Operator verification ("are all my executions indexed correctly?")

### 6.18 Worker Claim Count Reconciler

**Source:** RFC-009 (Scheduling, Fairness, and Admission), RFC-010 §4.8(h)

**Purpose:** Corrects the `current_claim_count` field on worker registration hashes. This count is maintained via async INCR/DECR (same pattern as quota concurrency, §6.6) and may drift due to lost INCR/DECR calls, worker crashes, or Valkey restarts.

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 10-30 seconds per worker |
| **Partition scope** | Cross-partition (reads from all `{p:N}` partitions for one worker) |
| **Operation** | For each active worker: sum `SCARD ff:idx:{p:N}:worker:<wid>:leases` across all execution partitions. Compare against `current_claim_count` on `ff:worker:<wid>`. If diverged, `HSET ff:worker:<wid> current_claim_count <actual>`. |
| **Per-item action** | `HSET` on the global worker hash. |
| **Failure mode** | Safe to crash. Stale count means capacity check may over-reject (safe direction) or under-reject (one extra claim). |
| **Idempotency** | Overwrites with authoritative count. Running twice is safe. |
| **Batch size** | 1 worker per cycle. Each worker requires `num_partitions` SCARD calls (parallelizable). |

---

## 7. Memory and Capacity Estimates

### 7.1 Per-Object Memory Estimates

All estimates assume Valkey hash ziplist encoding where applicable (small hashes use ziplist, large hashes use hashtable). Sizes are approximate and include Valkey overhead (key metadata, pointers, encoding headers). All core hashes (execution: 57 fields, attempt: ~25 fields, stream metadata: ~12 fields) stay within Valkey's default `hash-max-ziplist-entries` threshold of 128, ensuring compact ziplist representation.

#### Execution Core Hash

| Component | Fields | Avg Size |
|---|---|---|
| State vector (7 dimensions, stored as enum strings) | 7 | 70 bytes |
| Identity fields (execution_id, partition_id, namespace, lane_id, kind, idempotency_key) | 6 | 250 bytes |
| Timestamps (created, started, completed, last_transition, last_mutation, delay_until) | 6 | 96 bytes |
| Ownership fields (lease_id, lease_epoch, worker_id, worker_instance_id, lane, 7 lease timestamps/fields) | 12 | 450 bytes |
| Accounting fields (5 token/cost counters, retry/reclaim/replay/fallback counts, child counts) | 13 | 200 bytes |
| Relationship fields (parent, flow) | 2 | 80 bytes |
| Suspension fields (suspension_id, waitpoint_id) | 2 | 80 bytes |
| Attempt fields (attempt_index, total_attempt_count, current_attempt_id) | 3 | 80 bytes |
| Priority, progress (pct + message) | 3 | 120 bytes |
| Failure/cancel reasons | 2 | 100 bytes |
| Audit fields (creator, operator action, operator action timestamp) | 3 | 120 bytes |
| **Subtotal: execution core hash** | **~57 fields** | **~1.7 KB** |

#### Execution Sub-Keys

| Key | Avg Size | Notes |
|---|---|---|
| Payload (`ff:exec:...:payload`) | 1-10 KB | Depends on input size. JSON payloads typically 1-5 KB. |
| Result (`ff:exec:...:result`) | 0.5-5 KB | Present only on completed executions. |
| Policy snapshot (`ff:exec:...:policy`) | 0.5-1 KB | JSON-encoded ExecutionPolicySnapshot. |
| Tags (`ff:exec:...:tags`) | 0.1-0.5 KB | Typically 3-10 tags. |
| Claim grant (`ff:exec:...:claim_grant`) | 0.3 KB | Short-lived (5s TTL). Usually absent. |
| **Subtotal: execution sub-keys** | **~3-17 KB** | Highly variable by payload size. |

#### Attempt Records (Per Attempt)

| Key | Avg Size | Notes |
|---|---|---|
| Attempt hash (`ff:attempt:...:idx`) | 0.8 KB | ~25 fields. |
| Usage hash (`ff:attempt:...:idx:usage`) | 0.1 KB | 7 counter fields. |
| Policy snapshot (`ff:attempt:...:idx:policy`) | 0.4 KB | ~12 fields. |
| **Subtotal per attempt** | **~1.3 KB** | |

Most executions have 1-3 attempts. Heavily retried executions may have 5-10.

#### Attempt Index (Per Execution)

| Key | Avg Size | Notes |
|---|---|---|
| Attempts sorted set (`ff:exec:...:attempts`) | 0.1 KB per member | 48 bytes per ZSET entry (member + score). Typical: 1-3 entries. |

#### Lease Records (Per Lease)

| Key | Avg Size | Notes |
|---|---|---|
| Lease current (`ff:exec:...:lease:current`) | 0.6 KB | ~15 fields. Short-lived (lease TTL + grace). |
| Lease history (`ff:exec:...:lease:history`) | 0.2 KB per event | Valkey Stream. Typical: 3-10 events (acquire, renew×N, release). Capped by MAXLEN. |

#### Stream Records (Per Attempt With Streaming)

| Key | Avg Size | Notes |
|---|---|---|
| Stream metadata (`ff:stream:...:meta`) | 0.3 KB | ~12 fields. |
| Stream data (Valkey Stream) | **Variable** | Per frame: ~100-500 bytes (entry ID + fields). |

Typical LLM inference stream: 200-2000 token frames × ~150 bytes = **30-300 KB per attempt stream**.

Agent loop with tool events: 50-500 frames × ~300 bytes = **15-150 KB per attempt stream**.

#### Suspension Records (Per Suspension Episode)

| Key | Avg Size | Notes |
|---|---|---|
| Suspension current (`ff:exec:...:suspension:current`) | 0.8 KB | ~20 fields including JSON condition/policy. |
| Waitpoint (`ff:waitpoint:...:wpid`) | 0.5 KB | ~15 fields. |
| Waitpoint signals (ZSET or Stream) | 0.2 KB per signal | Typical: 1-5 signals per waitpoint. |
| Waitpoint condition (`ff:wp:...:condition`) | 0.3 KB | Matcher state. |

Most executions have 0-2 suspension episodes.

#### Index Entries (Per Execution, Amortized)

| Index | Per-Entry Size | Notes |
|---|---|---|
| Eligible sorted set | 48 bytes | execution_id member + priority score. |
| Delayed sorted set | 48 bytes | execution_id + delay_until score. |
| Active/lease_expiry sorted set | 48 bytes | execution_id + expires_at score. |
| Suspended sorted set | 48 bytes | execution_id + timeout_at score. |
| Terminal sorted set | 48 bytes | execution_id + completed_at score. |
| Namespace index | 48 bytes | execution_id + created_at score. |
| Per-execution signal index | 48 bytes per signal | signal_id + accepted_at score. |

An execution is in exactly one scheduling index at a time (eligible OR delayed OR active OR suspended OR terminal).

### 7.2 Total Per-Execution Memory Footprint

| Scenario | Attempts | Streaming | Suspension | Signals | Estimated Total |
|---|---|---|---|---|---|
| Simple fire-and-forget (no streaming, no suspension) | 1 | None | 0 | 0 | **~7.4 KB** |
| Single LLM call with token streaming | 1 | 500 frames | 0 | 0 | **~85 KB** |
| LLM call with 1 retry | 2 | 500 frames × 2 | 0 | 0 | **~175 KB** |
| Agent step with tool wait (suspension + signal) | 1 | 200 frames | 1 episode | 2 signals | **~45 KB** |
| Agent loop (5 steps, each with suspend/resume) | 1 | 1000 frames | 5 episodes | 10 signals | **~175 KB** |
| Heavily retried with fallback (5 attempts) | 5 | 300 frames × 5 | 0 | 0 | **~240 KB** |

### 7.3 Scale Projections

Assumes a mix: 60% simple (7.4 KB), 30% streaming (85 KB), 10% agent/complex (175 KB).
Weighted average: **~47 KB per execution**.

| Scale | Active Executions | Terminal (retained) | Total Execution Data | Index Overhead | Total Valkey Memory |
|---|---|---|---|---|---|
| 1K active | 1,000 | 10,000 | 520 MB | 5 MB | **~525 MB** |
| 10K active | 10,000 | 100,000 | 5.2 GB | 50 MB | **~5.3 GB** |
| 100K active | 100,000 | 1,000,000 | 52 GB | 500 MB | **~52 GB** |
| 1M active | 1,000,000 | 10,000,000 | 520 GB | 5 GB | **~525 GB** |

**Key insight:** Stream data dominates memory at scale. Aggressive stream retention (MAXLEN, short retention_ttl) is essential for 100K+ active executions. The `durable_summary` and `best_effort_live` durability modes (RFC-006, designed-for-deferred) will be critical at scale.

**Terminal retention impact:** The ratio of retained terminal executions to active ones dominates total memory. With 7-day retention and 1-hour average execution duration, the ratio is ~168:1. Aggressive terminal retention trimming (24-hour default for v1) is recommended.

### 7.4 Hot-Path Memory Considerations

| Hot path | Access pattern | Memory concern |
|---|---|---|
| Claim (scheduler) | ZRANGEBYSCORE on eligible set | Set size proportional to waiting executions per lane per partition. |
| Lease renewal | HSET on execution core + lease current | Two small writes. Negligible. |
| Usage report | HINCRBY on attempt usage + budget usage | Counter increments. Negligible. |
| Stream append | XADD on attempt stream | Frame payload size. Enforce 64KB max (RFC-006). |
| Signal delivery | HSET signal hash + ZADD indexes | Signal payload stored separately. |

No hot path requires loading the full execution record. The separation of core hash, payload, result, policy, and tags (RFC-001 §9.1) ensures hot-path operations touch minimal data.

### 7.5 Claim Throughput and Latency Budget

The claim flow is the highest-frequency critical path (§3.1). It crosses 3 partitions in 4 sequential Lua calls.

**Per-claim Valkey command count:**

| Step | Partition | Lua Script | Commands Inside | Notes |
|---|---|---|---|---|
| 1. Quota admission | `{q:K}` | `check_admission_and_record` | ~6 | TIME, ZREMRANGEBYSCORE, ZCARD, GET, ZADD, INCR |
| 2. Budget advisory read | `{b:M}` | (HMGET reads) | ~2 | HMGET usage, HMGET limits. No mutation. |
| 3. Issue claim-grant | `{p:N}` | `issue_claim_grant` | ~6 | HGETALL, EXISTS, ZRANK, HSET, PEXPIRE, ZREM |
| 4. Acquire lease | `{p:N}` | `claim_execution` | ~17 | HGETALL×2, HSET×4 (core, lease, attempt, usage, policy), ZADD×3, SADD, DEL×2, XADD, PEXPIREAT |
| **Total** | | **4 Lua calls** | **~31 commands** | |

**Per-claim latency budget:**

| Step | Network RTT | Lua Execution | Subtotal |
|---|---|---|---|
| 1. Quota (`{q:K}`) | 0.1 ms | 0.05-0.2 ms | ~0.2-0.3 ms |
| 2. Budget (`{b:M}`) | 0.1 ms | 0.02-0.1 ms | ~0.1-0.2 ms |
| 3. Grant (`{p:N}`) | 0.1 ms | 0.05-0.2 ms | ~0.2-0.3 ms |
| 4. Acquire (`{p:N}`, same shard) | 0 ms (pipelined) | 0.1-0.5 ms | ~0.1-0.5 ms |
| **Total claim latency** | | | **~0.6-1.3 ms** |

This is well within target for all v1 use cases: queue-compatible submission (typical: 1-10 ms), LLM inference (100 ms-30 s per call), agent steps (seconds per step).

**Throughput estimates by cluster size:**

| Cluster | Shards | Max Claims/sec | Rationale |
|---|---|---|---|
| Small prod | 3 | ~5K | Steps 3+4 both hit one `{p:N}` shard: ~23 cmds × 5K = 115K ops/shard. Comfortable. |
| Medium prod | 6-12 | ~20-40K | Partition distribution spreads claims across shards. |
| Large prod | 24-48 | ~50-100K | Each shard handles ~2-4K claims/sec worth of ops. Headroom for streaming and usage reporting. |

At scale, the bottleneck is the `{p:N}` shard handling steps 3+4 (~23 commands per claim). A single Valkey shard handles ~200-400K simple commands/sec but Lua scripts are heavier (block the event loop). Practical Lua throughput is ~50-100K script executions/sec per shard for scripts of this complexity.

---

## 8. Deployment Topology

### 8.1 Valkey Cluster Sizing

FlowFabric v1 is **Valkey-native**. The Valkey cluster is the single durable store for all execution, lease, attempt, stream, suspension, signal, flow, budget, and quota state.

**Recommended v1 setup by scale:**

| Scale | Shards | Replicas per Shard | Total Nodes | Estimated Memory per Shard |
|---|---|---|---|---|
| Dev/test (1K active) | 3 | 0 | 3 | ~175 MB |
| Small prod (10K active) | 3 | 1 | 6 | ~1.8 GB |
| Medium prod (100K active) | 6-12 | 1 | 12-24 | ~4-9 GB |
| Large prod (1M active) | 24-48 | 1-2 | 48-144 | ~11-22 GB |

**Shard count rationale:** Each shard handles ~16,384 / shard_count hash slots. More shards = more parallelism for Lua scripts (which block one shard at a time). At 100K+ active executions, Lua script latency on overloaded shards becomes the bottleneck.

**Replication:** At least 1 replica per shard for production. Replicas provide read scaling for inspection APIs (Class C operations) and failover durability.

**MAXMEMORY configuration (MANDATORY):**

| Setting | Required Value | Rationale |
|---|---|---|
| `maxmemory-policy` | `noeviction` | FlowFabric keys are durable state. Eviction would silently delete execution records, lease histories, or index entries, causing data loss and invariant violations. |
| Memory monitoring | Alert at 80% capacity | Provides operational headroom for traffic spikes and batch operations. |
| Hard alert | Alert at 90% capacity | Immediate operator action required: increase memory, reduce retention, or scale out. |
| Operational ceiling | Never exceed 90% | Above 90%, write-heavy Lua scripts risk OOM. Valkey Lua scripts are NOT transactional — a mid-script OOM aborts the script but prior writes within the script persist, leaving indexes inconsistent with execution state. The generalized index reconciler (§6.17) catches this, but prevention is better than repair. |

### 8.2 Partition Count Recommendations

Partitions are logical groupings that map to Valkey hash slots. They control which keys colocate for atomic Lua scripts.

| Partition Type | Tag Format | Recommended Count | Rationale |
|---|---|---|---|
| Execution partitions | `{p:N}` | 256-1024 | Must be >= shard count. Higher = more granular distribution. 256 is fine for small-medium, 1024 for large. |
| Flow partitions | `{fp:N}` | 64-256 | Fewer flows than executions. 64 is fine for small, 256 for large. |
| Budget partitions | `{b:M}` | 16-64 | Few budgets relative to executions. 16 is fine for most deployments. |
| Quota partitions | `{q:K}` | 16-64 | Same reasoning as budgets. |

**Partition assignment:** `partition_number = crc16(object_id) % num_partitions`. Computed once at object creation, immutable for lifetime.

**Hash slot mapping:** Valkey CRC16 on the hash tag `{p:N}` determines the shard. The partition count should be chosen so partitions distribute evenly across shards. If shard_count = 6 and partition_count = 256, each shard gets ~42-43 partitions.

### 8.3 Scheduler Process Placement

The scheduler is a stateless control-plane process that reads execution state, worker registrations, and resource policy, then issues claim-grants.

**Recommended v1 deployment:**

| Component | Instances | Placement |
|---|---|---|
| Scheduler | 2-3 (active-active) | Separate processes, co-located with or near the Valkey cluster. |
| Worker fleet | N (scaled to workload) | May be remote. Connect to Valkey via ferriskey cluster client. |

**Scheduler coordination:** Multiple scheduler instances may operate concurrently. The claim-grant model (RFC-009) prevents double-claims: the `issue_claim_grant.lua` script atomically removes the execution from the eligible set and writes the grant. If two schedulers race, one wins the ZREM and the other finds the execution missing from the set.

**No leader election required for v1.** Duplicate scheduling work is wasted but not incorrect. At very high scale, partition-affine scheduling (each scheduler owns a subset of partitions) can reduce contention.

### 8.4 Background Scanner Distribution

Background scanners should be distributed across processes to avoid single-point-of-failure and to parallelize partition scanning.

**Recommended v1 deployment:**

| Scanner | Process Affinity | Parallelism |
|---|---|---|
| Lease expiry scanner | Scheduler process or dedicated scanner | 1 scanner per partition shard (parallelize across shards). |
| Delayed execution promoter | Scheduler process | Same as lease expiry scanner. |
| Suspension timeout scanner | Scheduler process or dedicated scanner | 1 scanner per partition shard. |
| Pending waitpoint expiry | Scheduler process | Lower priority. Can share with suspension scanner. |
| Claim-grant reconciler | Scheduler process | 1 per scheduler instance (covers its own grants). |
| Budget/quota reconcilers | Dedicated reconciler process | 1 process, iterates budget/quota partitions sequentially. |
| Flow summary projector | Dedicated projector process | 1 per flow partition shard, or event-driven with periodic fallback. |
| Stream retention trimmer | Dedicated cleanup process | Low priority. 1 process, iterates execution partitions. |
| Terminal execution retention | Dedicated cleanup process | Low priority. Same process as stream trimmer. |
| Lease history trimmer | Inline (MAXLEN on XADD) | No separate process needed. |
| Budget reset scanner | Reconciler process | Low frequency. Shares with budget reconciler. |

**Minimum v1 process set:**
1. **Scheduler process** (2-3 instances): Handles claim scheduling, lease expiry scanning, delayed promotion, suspension timeout, pending waitpoint expiry, claim-grant reconciliation.
2. **Reconciler process** (1-2 instances): Handles budget/quota reconciliation, budget reset scanning.
3. **Projector process** (1 instance): Handles flow summary projection.
4. **Cleanup process** (1 instance): Handles stream retention, terminal retention, lease history trimming.

Total: **5-7 background processes** for a medium production deployment.

### 8.5 Client Connection Model

FlowFabric workers and control-plane processes connect to Valkey via the **ferriskey cluster client** (the Rust Valkey client being developed in this repository).

**Connection requirements:**
- Cluster-aware: automatic slot discovery, redirect handling, topology refresh.
- Pipeline support: batch commands for efficiency.
- Lua script support: EVALSHA with automatic script loading.
- TLS support: required for production (ElastiCache, cloud Valkey).

**Connection pool sizing:**
- Per worker: 2-4 connections (1 for claim/lease operations, 1 for stream append, 1-2 for reads).
- Per scheduler: 4-8 connections (higher concurrency for partition scanning).
- Per reconciler: 2-4 connections.

### 8.6 Background Scanner Capacity

Aggregate scanner overhead at recommended cluster sizes. Most scans find nothing at steady state (empty ZRANGEBYSCORE returns in ~50μs). Cost increases during bursts when scans find items and trigger Lua scripts.

**Aggregate scanner calls/sec (256 execution partitions):**

| Scanner | Frequency | Calls/s |
|---------|-----------|---------|
| Delayed promoter | 0.5-1s | 256-512 |
| Execution/attempt timeout | 1-2s | 128-256 |
| Lease expiry | 1-2s | 128-256 |
| Suspension timeout | 2-5s | 51-128 |
| Claim-grant reconciler | 5-10s | 26-51 |
| Pending waitpoint expiry | 5-10s | 26-51 |
| Dependency reconciler | 10-30s | 9-26 |
| Generalized index reconciler | 30-60s | 4-9 |
| All others (budget/quota/flow/stream/terminal) | 10-300s | ~20 |
| **Total** | | **~650-1300** |

**Per-shard overhead:**

| Scale | Shards | Partitions/shard | Scanner calls/shard/s | Overhead (@ 50μs/empty scan) | % of 1 core |
|-------|--------|------------------|----------------------|------------------------------|-------------|
| 10K active | 3 | ~85 | ~220-430 | 11-22ms/s | ~1-2% |
| 100K active | 12 | ~21 | ~55-110 | 3-6ms/s | <1% |
| 1M active | 48 | ~5 | ~13-27 | 0.7-1.4ms/s | <0.1% |

Scanner overhead is negligible at all scales. The delayed promoter is the most frequent scanner — consider increasing its interval to 1-2s if Lua script contention becomes measurable.

---

## 9. V1 Scope and Non-Goals for the Backend

### 9.1 V1 Must-Have

The Valkey backend must support in v1:

**Core execution lifecycle:**
- Execution CRUD with orthogonal state vector (all 7 dimensions).
- Atomic state transitions via Lua scripts (claim, complete, fail, cancel, expire, skip, suspend, resume).
- Idempotent execution creation via SET NX.

**Lease correctness:**
- Acquire, renew, release, revoke, reclaim — all via atomic Lua scripts.
- Monotonic `lease_epoch` fencing.
- Lease expiry detection and reclaimability.

**Attempt lifecycle:**
- Create, start, suspend, resume, end, interrupt — all attempt states including `suspended`.
- Per-attempt usage, policy snapshot, and stream.
- Attempt index sorted set per execution.

**Scheduling and admission:**
- Per-lane eligible/delayed/active/suspended/terminal sorted sets.
- Priority-scored claiming via ZPOPMIN-style access.
- Claim-grant model with TTL.
- Worker registration with heartbeat.

**Suspension and signals:**
- Atomic suspend (lease release + suspension record + waitpoint creation).
- Pending waitpoint pre-creation for callback races.
- Signal delivery with resume condition evaluation.
- Waitpoint closure and execution resume — all atomic.

**Streaming:**
- Valkey Streams for per-attempt output.
- Lease-validated append with 64KB frame limit.
- XRANGE reads, XREAD BLOCK tailing.
- MAXLEN retention trimming.

**Flow coordination:**
- Flow container with membership, edges, and policy.
- Dependency resolution with skip propagation.
- Two-partition model (flow structural + execution-local eligibility).

**Budget and quota:**
- Budget usage increment with atomic breach check (colocated scopes).
- Best-effort-consistent cross-partition budget enforcement.
- Sliding-window rate limiting.
- Concurrency caps with async reconciliation.

**Background processes:**
- All 12 scanners/reconcilers listed in §6.
- Idempotent, crash-safe, partition-parallel.

### 9.2 Designed-For but Deferred

**SQLite local mode:** The partition model and key schema are designed to be implementable on a single-node SQLite backend for local development and testing. Deferred because Valkey is the primary backend and the abstraction layer is not yet built.

**Multi-backend abstraction:** The operation semantics (atomicity classes A/B/C) are defined abstractly enough to support alternative backends (PostgreSQL, DynamoDB). Deferred because v1 is Valkey-native and the interface boundary is not yet hardened.

**Global secondary indexes:** Cross-partition search by tag, tenant, time range, or arbitrary field. V1 uses partition-local indexes and namespace-level sorted sets. Full-text or faceted search requires an external index (Elasticsearch, Meilisearch).

**Pub/sub for real-time notifications:** Valkey SUBSCRIBE channels for execution completion notifications, stream frame push, and flow state changes. V1 uses polling (XREAD BLOCK for streams, periodic reads for state). Pub/sub can reduce latency for request/reply and live dashboards.

**AOF/RDB persistence tuning:** V1 uses Valkey defaults or cloud provider defaults (e.g., ElastiCache automatic backups). Production persistence tuning (fsync policy, snapshot frequency, memory fragmentation management) is operational, not architectural.

**Public API wire format (gRPC/REST/SDK):** V1 operations are invoked via Valkey Lua scripts through the ferriskey cluster client. No public API wire format is specified in these RFCs. An API server translating external gRPC/REST requests to Lua script invocations is a v1 companion component — its design is outside the scope of the execution engine RFCs. UC-62 (language-neutral control API) is addressed by the operation semantics defined here; the transport layer is deferred.

**Cross-execution event feed:** V1 does not provide a lane-level or system-level event stream for observers and dashboards. Per-execution observability is available: attempt streams (XREAD BLOCK for live output, RFC-006), lease history streams (lifecycle events, RFC-003). Lane-level dashboards rely on periodic count aggregation (§6.13). A cross-execution event stream (one XADD per state transition, consumable by dashboards and webhooks) is a designed-for extension that would address UC-55 (execution event feed) more fully.

**Unified per-execution audit trail:** V1 audit trail is reconstructable from four sources: lease history (ownership events with `reason` field — completed, failed, suspended), attempt records (per-attempt timing and outcome), signal records (delivery and resume effects), and suspension records (episodes and close reasons). A unified per-execution event stream (one XADD per lifecycle transition: created, claimed, completed, failed, cancelled, suspended, resumed, replayed, operator_override) is a designed-for extension that would consolidate audit into one queryable timeline.

### 9.3 Retention and Cleanup Strategy

**V1 retention defaults:**

| Object | Default Retention | Configurable? |
|---|---|---|
| Terminal execution (core + sub-keys) | 24 hours after completion | Per-lane policy |
| Attempt records | Same as parent execution | Inherited |
| Stream data (closed streams) | 24 hours after stream close | Per-lane `stream_policy.retention_ttl_ms` |
| Stream data (open streams) | MAXLEN 10,000 frames | Per-lane `stream_policy.retention_maxlen` |
| Lease history | MAXLEN 100 events per execution | Global default |
| Signal records | Same as parent execution | Inherited |
| Suspension/waitpoint records | Same as parent execution | Inherited |
| Flow records | 24 hours after flow completion | Per-flow or per-namespace policy |
| Budget records | Indefinite (operator-managed) | Operator deletes explicitly |
| Quota policy records | Indefinite (operator-managed) | Operator deletes explicitly |
| Worker registrations | Heartbeat TTL (e.g., 60s) | Per-worker |
| Claim grants | 5 seconds TTL | Global default |
| Idempotency keys | Dedup window TTL (e.g., 24h) | Per-lane |
| Waitpoint key resolution | Waitpoint TTL + grace | Per-waitpoint |

**Cleanup cascade:** When a terminal execution is purged by the retention scanner, all sub-objects are deleted in one batch:
1. Execution core hash, payload, result, policy, tags.
2. All attempt hashes, usage hashes, policy hashes (discovered via `ff:exec:...:attempts` ZSET).
3. Attempt sorted set.
4. All stream data and metadata (per attempt, discovered via attempts ZSET).
5. Lease current (if still present) and lease history.
6. Suspension current record.
7. Per-execution signal index and individual signal records **including payload keys** (`ff:signal:{p:N}:<signal_id>` + `ff:signal:{p:N}:<signal_id>:payload`, discovered via `ff:exec:...:signals` ZSET).
8. Dependency records: `deps:meta` hash, all `dep:<edge_id>` hashes (discovered via SCAN `ff:exec:{p:N}:<exec_id>:dep:*`), `deps:unresolved` set.
9. All waitpoint records: for each waitpoint_id in `ff:exec:...:waitpoints` SET (mandatory), DEL the waitpoint hash (`ff:wp:{p:N}:<wp_id>`), signals stream (`ff:wp:{p:N}:<wp_id>:signals`), and condition hash (`ff:wp:{p:N}:<wp_id>:condition`). Then DEL the waitpoints set itself.
10. ZREM from terminal sorted set, namespace index, tag indexes.

This cascade is implemented as a Lua script per partition (all keys share `{p:N}`), ensuring atomic cleanup within the partition. The script reads the execution core first (to extract flow_id, lane_id, namespace, tags), then the attempts ZSET (for attempt/stream keys), the signals ZSET (for signal keys), and the waitpoints SET (for waitpoint keys), before deleting everything.

Cross-partition references (flow membership, budget attachment) are cleaned lazily: the flow summary projector (§6.7) SREM's stale members on member-not-found, and budget reconcilers remove stale execution references.

### 9.4 Explicit Non-Goals

The following are **not** goals for the v1 Valkey backend:

- **Durable event sourcing:** FlowFabric stores current state + audit history, not a complete event log that can reconstruct state from scratch. Lease history and flow events provide audit trails, but the execution core hash is the source of truth, not a derived projection.
- **Cross-shard transactions:** Valkey Cluster does not support multi-slot transactions. All atomic operations are partition-local. Cross-partition consistency is achieved through idempotent reconciliation, not distributed transactions.
- **Zero-downtime schema migration:** V1 key schemas are designed for forward compatibility (new fields can be added to hashes without migration), but structural changes (e.g., changing partition count) require a planned migration.
- **Built-in observability backend:** FlowFabric exposes metrics and events for external observability (Prometheus, Grafana, OpenTelemetry), but does not include a built-in dashboard or time-series store.
- **Namespace-based access control:** V1 does not enforce namespace/tenant isolation at the engine level. The `namespace` field on execution, flow, and budget objects is metadata used for fairness (RFC-009 §5.3), listing (namespace index), and idempotency scoping — **not a security boundary**. Any caller who knows an `execution_id` can read, signal, or inspect that execution regardless of namespace. Multi-tenant isolation must be enforced at the API gateway or control plane layer. Engine-level namespace enforcement is a designed-for extension.
- **Separate dead-letter queue:** FlowFabric does not have a DLQ. Terminal failed executions are stored in the per-lane terminal sorted set (`ff:idx:{p:N}:lane:<lane_id>:terminal`) alongside completed/cancelled/expired/skipped executions. Operators find failed executions via `list_executions(lane_id, public_state=failed)`. The `failure_category` field on the attempt record (RFC-002) distinguishes failure causes (`timeout`, `worker_error`, `provider_error`, `budget_exceeded`). Failed executions are replayable via `replay_execution` (RFC-001 §4.4). This design matches UC-07 (dead-letter handling): "move terminal failures into a durable failed state for inspection or replay."

---

## 10. Worker SDK Contract

Single coherent reference for SDK authors implementing a FlowFabric worker.

### 10.1 Minimum Viable Worker Loop

```
STARTUP:
  register_worker(worker_id, instance_id, capabilities)     [RFC-009]
  start heartbeat_worker(instance_id) every 10-30s          [RFC-009]

CLAIM:
  1. claim_request(caps, lanes) → scheduler issues grant    [RFC-009]
  2. IF attempt_state == attempt_interrupted:
       claim_resumed_execution → same attempt, new lease    [RFC-001]
     ELSE:
       claim_execution → new attempt + lease                [RFC-001]

PROCESS (loop while working):
  3. renew_lease(exec_id, lease_id, epoch) every ttl/3      [RFC-003]
  4. append_frame(exec_id, att_idx, lease_id, epoch, frame) [RFC-006]
  5. report_usage(exec_id, att_idx, epoch, delta)           [RFC-008]

DONE:
  6a. complete_execution(exec_id, lease_id, epoch, att_id)  [RFC-001]
  6b. fail_execution(exec_id, lease_id, epoch, att_id, ...) [RFC-001]

SUSPEND (external waits):
  7a. create_pending_waitpoint → returns waitpoint_key      [RFC-004]
  7b. call external system with waitpoint_key
  7c. suspend_execution → releases lease, exits loop        [RFC-004]
      on resume: re-claim via step 2 (claim_resumed)

SHUTDOWN:
  deregister_worker(instance_id) — leases expire naturally  [RFC-009]
```

### 10.2 Error Handling

| Error | Action |
|-------|--------|
| `no_eligible_execution` | Backoff 100ms-1s, retry. |
| `claim_grant_expired` | Retry from step 1. |
| `stale_lease` / `lease_expired` / `lease_revoked` | Stop. Do NOT complete/fail. Reclaimed. |
| `execution_not_active` | On retry: check enriched return. Epoch match + success = my call won. |
| `budget_exceeded` | Per policy: fail/suspend/warn. |

### 10.3 SDK Requirements

- Cluster-aware Valkey client: slot discovery, redirects, EVALSHA.
- Independent heartbeat thread: must run even if processing blocks.
- Independent lease renewal thread: must fire during slow LLM calls.
- Partition awareness: track `partition_id` per execution for `{p:N}` keys.
- Parse structured returns per §4.9: `{1, "OK", ...}` or `{0, "ERROR", ...}`.

---

## References

- RFC-001: Execution Object and State Model
- RFC-002: Attempt Model and Execution Lineage
- RFC-003: Lease and Fencing Semantics
- RFC-004: Suspension and Waitpoint Model
- RFC-005: Signal Model and Control Boundary
- RFC-006: Stream Model
- RFC-007: Flow Container and DAG Semantics
- RFC-008: Budget, Quota, and Rate-Limit Policy
- RFC-009: Scheduling, Fairness, and Admission
