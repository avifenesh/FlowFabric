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
| `ff:exec:{p:N}:<execution_id>:suspensions` | STREAM or ZSET | **Designed-for v2.** No v1 script writes to this key. | Reserved for v2 unified event stream (§9.2). Historical suspension records for executions with multiple episodes. Implementers may skip creating this key until the unified event model is implemented. |

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
| `ff:idx:{p:N}:worker:<worker_instance_id>:leases` | SET | 001, 003 | Member: `execution_id`. | Per-worker lease set within partition. Operator drain aid. Best-effort. **Worker→execution lookup:** iterate across all partitions. Monitoring layer should cache globally via lease history events. |
| `ff:idx:{p:N}:suspension_timeout` | ZSET | 004 | Member: `execution_id`. Score: `timeout_at` (ms). | Suspension timeout scanner target. Cross-lane within partition. |
| `ff:idx:{p:N}:pending_waitpoint_expiry` | ZSET | 004 | Member: `waitpoint_id`. Score: `expires_at` (ms). | Pending waitpoint cleanup scanner target. |
| `ff:idx:{p:N}:attempt_timeout` | ZSET | 001, 010 | Member: `execution_id`. Score: `started_at + timeout_ms`. | Per-attempt timeout. ZADD on `claim_execution`/`claim_resumed_execution`. ZREM on `complete`/`fail`/`expire`. Scanned by §6.16. |
| `ff:idx:{p:N}:execution_deadline` | ZSET | 001, 010 | Member: `execution_id`. Score: `deadline_at` from timeout_policy. | Total execution deadline. ZADD on `create_execution` (if deadline set). ZREM on terminal. Scanned by §6.16. |
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

##### Flow-Partition Indexes — `{fp:N}`

| Key Pattern | Type | RFC | Score / Member | Notes |
|---|---|---|---|---|
| `ff:idx:{fp:N}:terminal_flows` | ZSET | 010 | Member: `flow_id`. Score: flow terminal transition timestamp (ms). | ZADD when `public_flow_state` transitions to `completed`, `failed`, or `cancelled`. Scanned by the flow retention scanner (§6.19). |

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
| `ff:quota:{q:K}:<quota_policy_id>:window:<dimension>` | ZSET | 008 | Entries added on admission. Stale entries removed by `ZREMRANGEBYSCORE` on each check. | Sliding window. Member: `execution_id` (not `execution_id:timestamp` — changed in R22 for idempotent retry, see script #32). Score: `timestamp_ms`. ZADD on existing member updates score without creating a duplicate entry. |
| `ff:quota:{q:K}:<quota_policy_id>:concurrency` | STRING | 008 | Created on first INCR. INCR on lease acquire, DECR on lease release (async). | Active concurrency counter. Subject to drift; reconciled periodically. |
| `ff:quota:{q:K}:<quota_policy_id>:admitted:<execution_id>` | STRING | 008 | SET NX on admission with TTL = window_size. Auto-expires. | Idempotency guard for `check_admission_and_record`. Prevents double-INCR of concurrency counter on retry. Value: `"1"`. If key exists, script returns `ALREADY_ADMITTED` without mutation. |
| `ff:quota_attach:{q:K}:<scope_type>:<scope_id>` | SET | 008 | Created on `attach_quota_policy`. | Forward index: scope → attached quota_policy_ids. |

#### 1.5 Global Keys (No Hash Tag)

These keys have no hash tag and land on a shard determined by the full key string. They are few, read-heavy, and written infrequently — acceptable for global placement.

| Key Pattern | Type | RFC | Lifecycle | Notes |
|---|---|---|---|---|
| `ff:lane:<lane_id>:config` | HASH | 009 | Created on lane creation. Updated on lane config changes. | Lane configuration: state, default policies, scheduling weight, max concurrency. |
| `ff:lane:<lane_id>:counts` | HASH | 009 | Created with lane. Updated periodically by background aggregator. | Derived cached counts by public_state. Not authoritative — ZCARD on partition sets is authoritative but slower. |
| `ff:worker:<worker_instance_id>` | HASH | 009 | Created on `register_worker`. TTL based on `last_heartbeat_at + worker_ttl_ms`. Auto-expires on heartbeat failure. | Worker registration: capabilities, status, capacity, heartbeat. |
| `ff:idx:workers` | SET | 009 | SADD on `register_worker`, SREM on `deregister_worker`. Members auto-stale when worker TTL expires (scheduler checks `ff:worker:<wid>` EXISTS before using). | All registered worker instance IDs. Scheduler reads on startup to enumerate workers. |
| `ff:idx:workers:cap:<key>:<value>` | SET | 009 | Updated atomically with worker registration. Members removed on deregistration or TTL expiry. | Capability index: which workers have a given capability. Member: `worker_instance_id`. |
| `ff:sched:fairness:lane:<lane_id>:deficit` | STRING | 009 | Created by scheduler. TTL: `fairness_window_ms * 2`. | Ephemeral. Weighted deficit for cross-lane round-robin. Rebuilt from queue depths on restart. |
| `ff:sched:fairness:tenant:<namespace>:lane:<lane_id>:claims` | STRING | 009 | Created by scheduler. TTL: `fairness_window_ms`. | Ephemeral. Tenant claim count in current fairness window. |
| `ff:idx:lanes` | SET | 009 | SADD on `create_lane`, SREM on `delete_lane`. | Lane registry. Members: `lane_id`. Scheduler reads at startup + periodic refresh for cross-lane fairness discovery. |

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

Every operation that spans more than one partition family. These multi-step sequences are implemented in `ff-engine::dispatch` (for engine-side orchestration) or `ff-sdk` (for worker-side orchestration via ff-engine). The exception is the claim flow (§3.1), which is implemented in `ff-scheduler` — the scheduler owns its own dispatch for the claim sequence and does NOT depend on ff-engine.

For each operation: the partition sequence, what happens at each step, failure modes, and convergence mechanism.

#### 3.1 Three-Partition Claim Flow (`ff-scheduler`)

**Operation:** Worker claims an eligible execution. **Implemented in `ff-scheduler`** — the scheduler owns this 5-step sequence and calls `ff-script` FCALL wrappers directly. `ff-scheduler` does NOT depend on `ff-engine`.
**Partitions touched:** `{q:K}` → `{b:M}` (per-candidate) → `{p:N}` (→ `{p:N}` again for `acquire_lease`)

```
Worker                Scheduler                {q:K}              {b:M}             {p:N}
  │                      │                       │                  │                 │
  ├─claim_request──────►│                       │                  │                 │
  │                      ├─check_admission─────►│                  │                 │
  │                      │◄──────admitted────────│                  │                 │
  │                      │ SELECT CANDIDATE      │                  │                 │
  │                      ├─check_budget (per-candidate)────────►│                 │
  │                      │◄──────────────────────────budget_ok──│                 │
  │                      ├─issue_claim_grant──────────────────────────────────────►│
  │                      │◄──────────────────────────────────────────grant_issued──│
  │                      │                       │                  │                 │
  ├─acquire_lease (Lua)───────────────────────────────────────────────────────────►│
  │◄──────────────────────────────────────────────────────────────────lease+attempt─│
```

**Step 1 — Quota pre-check (`{q:K}`) — READ-ONLY**
- Scheduler reads `ff:quota:{q:K}:<policy_id>:window:<dim>` (ZREMRANGEBYSCORE + ZCARD) and `concurrency` (GET). Compares against cached limits. **No mutation** — no ZADD, no INCR. The write-path `check_admission_and_record` (ZADD + INCR) runs at step 4a below, after candidate selection provides an execution_id.
- If window full or concurrency cap reached: return rejection to worker. Stop.
- If denied: return `rate_limit_exceeded` or `concurrency_limit_exceeded`. Stop.

**Step 2 — Select candidate**
- Scheduler reads eligible sorted sets across partitions for the selected lane(s).
- Applies fairness policy (cross-lane weighted round-robin, cross-tenant fair share).
- Selects the highest-priority, capability-matched candidate.

**Step 3 — Budget check per-candidate (`{b:M}`)**
- For the selected candidate: read `budget_ids` from the execution's core hash (cached from prior partition scan or read inline). For each attached budget: check the **per-cycle budget cache** first. If not cached, read `ff:budget:{b:M}:<budget_id>:usage` and `:limits`, cache the result keyed by `budget_id`.
- Advisory read — no mutation. Usage is not pre-charged.
- If any attached budget's hard limit is already breached:
  - Run `ff_block_execution_for_admission` on the candidate's `{p:N}` partition. This atomically sets `eligibility_state = blocked_by_budget`, `blocking_reason = waiting_for_budget`, ZREMs from eligible, ZADDs to `blocked:budget`.
  - **Go back to step 2** with the next candidate. If no more candidates: return empty response to worker.
- If all budgets have headroom: proceed.

**Budget cache per scheduling cycle:** The scheduler caches budget breach status per `budget_id` during a single claim cycle. The first candidate sharing budget B triggers a cross-partition read to `{b:M}`; all subsequent candidates sharing B reuse the cached result. Cache invalidated at cycle end. This prevents O(N) reads to the same `{b:M}` hash when N candidates share a breached budget. Same caching pattern as the unblock scanner (§6.15).

**Step 4a — Quota admission record (`{q:K}`) — WRITE**
- Now that a candidate is selected (execution_id known): run `ff_check_admission_and_record` on `{q:K}`.
- This is the write-path: ZADD execution_id to window, SET NX admitted guard, INCR concurrency.
- If the rate limit or concurrency cap was hit between the step 1 read and now: returns `RATE_EXCEEDED` or `CONCURRENCY_EXCEEDED`. Go back to step 2 with next candidate.
- If `ALREADY_ADMITTED` (idempotent retry): proceed.
- If `ADMITTED`: proceed.

**Step 4b — Issue claim-grant (`{p:N}`)**
- `FCALL ff_issue_claim_grant` on the execution's partition.
- Validates execution is still `runnable` + `eligible_now` + `unowned`.
- Writes `ff:exec:{p:N}:<execution_id>:claim_grant` with TTL.
- ZREM from eligible sorted set (prevents double-grant).
- If execution already claimed/cancelled: return error. Stop.

**Step 5 — Acquire lease (`{p:N}`)**
- `FCALL ff_claim_execution` (or `ff_claim_resumed_execution`) on the same partition as step 4b.
- Validates and consumes the claim grant.
- Creates lease, creates attempt, transitions execution to `active`.
- Updates all partition-local indexes atomically.
- If grant expired or mismatched: return error.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Step 1 rejected (quota) | No side effects. Worker retries or backs off. | Immediate — no cleanup needed. |
| Step 3 rejected (budget) — candidate blocked | Step 1 INCR'd. Candidate moved to `blocked:budget`. Scheduler tries next candidate. | **Stale INCR** if no candidate succeeds. Concurrency reconciler corrects. Blocked candidate unblocked by §6.15 when budget frees. |
| Step 3 denied for ALL candidates | Step 1 INCR'd. All candidates blocked. Worker gets empty response. | Same INCR drift. All candidates recoverable via unblock scanner. |
| Step 4 fails (execution gone) | Step 1 INCR'd, step 3 passed. | Same INCR drift. No grant written — no orphan. |
| Step 4 succeeds, step 5 never runs (worker crash) | Grant key exists with TTL. Execution removed from eligible set. | Grant TTL expires (5s). **Grant expiry reconciler** (RFC-009 §12.8) detects execution is `runnable` + `eligible_now` but not in eligible set, re-adds it. |
| Step 4 succeeds, step 5 fails (state changed between 4 and 5) | Grant exists but execution no longer claimable. | Grant TTL expires. Reconciler checks — no longer `runnable`, no re-add needed. |
| **Quota partition `{q:K}` unreachable** | Scheduler cannot check admission. | **FAIL-OPEN.** Proceed without quota check. Over-admission bounded by claim-grant serialization on `{p:N}`. Reconcile when `{q:K}` recovers. |
| **Budget partition `{b:M}` unreachable** — hard-limit budget | Scheduler cannot check budget for selected candidate. | **FAIL-CLOSED.** Do not issue claim grant. Execution remains eligible but unclaimed until `{b:M}` recovers. |
| **Budget partition `{b:M}` unreachable** — advisory/soft budget | Scheduler cannot check budget for selected candidate. | **FAIL-OPEN.** Proceed without budget check. Log warning. |
| **Execution partition `{p:N}` unreachable** | Cannot access execution state at all. | **FAIL.** Claim impossible — skip until partition recovers. |

#### 3.2 Flow Dependency Resolution (`ff-engine::dispatch`)

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
2. **For each edge, resolve on child's `{p:N}`**: `FCALL ff_resolve_dependency`. If upstream succeeded: mark edge `satisfied`, decrement unresolved count, if all resolved → set `eligible_now` + add to eligible sorted set. If upstream failed: mark edge `impossible`, increment impossible count → set execution to terminal `skipped`.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Step 1 (stage) succeeds, step 2 (apply) fails | Edge staged but not applied. Child has no gating record. | The mutation grant has a TTL. Control plane retries step 2 using the same grant (idempotent — `apply_dependency_to_child` checks `EXISTS` before writing). |
| Step 2 succeeds, step 3 (finalize) fails | Edge applied to child but still `pending` on flow. Child is correctly gated. | Control plane retries step 3. `graph_revision` ensures stale finalizations are rejected. |
| Resolve: flow read succeeds, child resolution fails for one child | Some children resolved, some not. | Engine retries failed child resolutions. `resolve_dependency` is idempotent — already-resolved edges return `already_resolved`. |

#### 3.3 Budget Enforcement on Usage Report (`ff-engine::dispatch`, called by `ff-sdk`)

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

**For flow/lane/tenant-scoped budgets (on `{b:M}`):** After the `{p:N}` attempt usage update, the control plane issues a separate `FCALL ff_report_usage_and_check` to each `{b:M}` partition. If a hard breach is detected, the control plane applies the enforcement action (fail/suspend) back on `{p:N}` in a separate call.

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

#### 3.5 Async Quota Concurrency Decrement (`ff-engine::dispatch`)

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

#### 3.6 Flow Summary Projection (`ff-engine::scanner`)

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

#### 3.8 Add Execution to Flow (`ff-engine::dispatch`)

**Operation:** Add an execution to a flow's membership. Enforces single-flow invariant (F2) cross-partition.
**Partitions touched:** (optional `{b:M}` budget check) → `{p:N}` (execution) → `{fp:N}` (flow)

1. **Budget check on `{b:M}`** (if flow has `deny_child` budget): read budget usage. If exhausted, reject.
2. **Phase 1 — ownership claim on `{p:N}`**: Lua script atomically checks `flow_id` field on exec core. If set to a *different* flow → `execution_already_in_flow`. If set to *this* flow → idempotent, skip SET and proceed (supports `create_child_execution` which pre-sets `flow_id`). If empty → SET `flow_id = <flow_id>`. This is the serialization point that prevents two flows from claiming the same execution.
3. **Phase 2 — membership on `{fp:N}`**: SADD to flow membership set. Increment `graph_revision`. Apply flow policy defaults. Idempotent — if this step fails and is retried, SADD on an existing member is a no-op.

**After Phase 2 + edge staging:** For any root member with zero dependencies, the control plane calls `promote_blocked_to_eligible` (#35) immediately. This avoids the 10-60s reconciler delay for the first execution in the flow to become claimable.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Phase 1 succeeds, phase 2 fails | Execution has `flow_id` set but is not in the flow's membership set. | Retry phase 2. SADD is idempotent. Flow summary projector will eventually detect the member when it reads exec core. |
| Two concurrent adds to different flows | Both reach phase 1 on the same `{p:N}` — serialized. First wins, second gets `execution_already_in_flow`. | Immediate — {p:N} serialization prevents the race. |

#### 3.9 Flow Cancellation Propagation (`ff-engine::dispatch`)

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
  │  ff_cancel_execution   │                         │
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
| `let_active_finish` | Block unclaimed runnable members (`blocked_by_operator` + `paused_by_flow_cancel`). | Active members finish. `cancel_flow` is sole authority for clearing `paused_by_flow_cancel` → `terminal(cancelled)`. |
| `coordinator_decides` | Coordinator execution determines scope. | Per-branch decision. |

**Per-member cancellation:** Each `cancel_execution` runs on the member's `{p:N}` partition. The operation is idempotent — cancelling an already-terminal execution is a no-op.

**`let_active_finish` uses `paused_by_flow_cancel`** (not `paused_by_policy`) to prevent the unblock scanner (§6.15) from erroneously unblocking flow-cancelled executions when the lane is open.

**Failure modes and convergence:**

| Failure Point | Effect | Convergence |
|---|---|---|
| Flow members read succeeds, some per-member cancellations fail | Partial propagation. Some members remain non-cancelled. | Engine retries failed cancellations. `cancel_execution` is idempotent — already-cancelled members return success. |
| Flow state update fails after members are cancelled | Members cancelled but flow still shows `running`. | Flow summary projector detects all members terminal and updates `public_flow_state` on next cycle. |
| `let_active_finish`: active members crash after blocking unclaimed | Unclaimed members stuck in `blocked_by_operator`. Active members eventually reclaimed or timed out. | When no active members remain (detected by flow summary projector), cancel the blocked members. |

---

### 4. Background Scanners and Reconcilers

Every periodic background process required by the architecture, consolidated in one place. **All scanners are implemented in `ff-engine::scanner`.** There is no separate scanner process — scanners run as tokio tasks within `ff-server`. Each scanner is a `tokio::spawn`'d loop that pipelines across partitions using `ferriskey`.

**Scanner timing model:** Frequencies listed are **wall-clock intervals per partition**, but partitions are scanned via **pipelined batches**, not sequentially. A scanner with "1s per partition" and 256 partitions does NOT take 256s. Implementation: pipeline `ZRANGEBYSCORE` across all partitions in batches (pipeline depth ~32-64). With 256 partitions and pipeline depth 32: ~8 batches × network RTT (~1ms) = ~8ms total scan time for empty results. Processing found items adds Lua script execution time per item. **Total cycle time for all P partitions = ceil(P / pipeline_depth) × (RTT + per_item_processing).** For 256 partitions with empty results: < 50ms. The frequency defines the **sleep interval between cycles**, not per-partition sequential delay.

**Short timeout note:** For deployments with per-attempt timeouts < 5s, the execution/attempt timeout scanner frequency SHOULD be reduced to 0.5s to limit overshoot to ~10%. The default 1-2s frequency causes up to 40% overshoot on 5s timeouts (7s actual vs 5s configured). Alternatively, implement inline timeout checking in `renew_lease` (compare `now_ms` against attempt_timeout score on each renewal).

| Scanner | Target Key | Frequency | Action | RFC |
|---|---|---|---|---|
| **Delayed → Eligible promotion** | `ff:idx:{p:N}:lane:<lane_id>:delayed` | 0.5–1s per partition | `ZRANGEBYSCORE -inf <now>`. Verify execution state, set `eligible_now`, ZADD eligible set with priority score. | 001, 009 |
| **Lease expiry / reclaim** | `ff:idx:{p:N}:lease_expiry` | 1–2s per partition | `ZRANGEBYSCORE -inf <now>`. Run `FCALL ff_mark_lease_expired_if_due` or `FCALL ff_reclaim_execution`. Re-validates execution core; no-op if lease was renewed. | 003 |
| **Suspension timeout** | `ff:idx:{p:N}:suspension_timeout` | 2–5s per partition | `ZRANGEBYSCORE -inf <now>`. Run `FCALL ff_expire_suspension`. Applies timeout behavior (fail/cancel/expire/auto_resume/escalate). | 004 |
| **Pending waitpoint expiry** | `ff:idx:{p:N}:pending_waitpoint_expiry` | 5–10s per partition | `ZRANGEBYSCORE -inf <now>`. Close waitpoints with `close_reason = never_committed`. | 004 |
| **Claim-grant expiry reconciler** | Execution core records | 5–10s per partition | Find executions where `lp = runnable` AND `es = eligible_now` but not in any eligible sorted set. Re-add if no grant key exists. | 009 |
| **Budget counter reconciler** | `ff:budget:{b:M}:<budget_id>:usage` | 30–60s per budget partition | Sum actual attempt-level usage across attached executions. Compare against budget usage hash. Correct if diverged. | 008 |
| **Quota concurrency reconciler** | `ff:quota:{q:K}:<policy_id>:concurrency` | 10–30s per quota scope | Count actual active leases for the scope (walk `{p:N}` partition worker-lease sets). Reset counter if drifted. | 008 |
| **Budget reset** | `ff:idx:{b:M}:budget_resets` | 10–30s per budget partition | `ZRANGEBYSCORE -inf <now>`. Run `FCALL ff_reset_budget`. Zero usage, set `next_reset_at`, re-score in index. | 008 |
| **Flow summary projector** | `ff:flow:{fp:N}:<flow_id>:summary` | Event-driven + 10–30s catchup | Consume execution state-change events. Update member counts and aggregate usage. Derive `public_flow_state`. | 007 |
| **Lease history trimmer** | `ff:exec:{p:N}:<execution_id>:lease:history` | Inline (`MAXLEN ~` on XADD) + 60–120s background | Inline: on every lease history append. Background: scan for streams exceeding size threshold and apply aggressive trim. | 003 |
| **Stream time-based cleanup** | `ff:stream:{p:N}:*:meta` | 60s per partition | Check `closed_at + retention_ttl_ms < now`. Delete stream + meta keys. | 006 |
| **Terminal retention** | `ff:idx:{p:N}:lane:<lane_id>:terminal` | Minutes–hours | `ZRANGEBYSCORE -inf <retention_cutoff>`. Purge execution + all sub-keys (cascade delete). | 001, 002, 005, 006 |
| **Worker heartbeat expiry** | `ff:worker:<worker_instance_id>` | Handled by TTL | Valkey auto-expires worker registration keys when heartbeat TTL lapses. Scheduler skips expired workers. | 009 |
| **Dependency resolution reconciler** | `ff:idx:{p:N}:lane:<lane_id>:blocked:dependencies` | 10–30s per partition | For each blocked execution: read upstream terminal states (cross-partition). If upstream is terminal, run `FCALL ff_resolve_dependency`. Safety net for engine crash between parent complete and child resolve dispatch. | 007, 010 |
| **Lane count aggregator** | `ff:lane:<lane_id>:counts` | 5–15s per lane | ZCARD across all `{p:N}` partitions for each per-lane sorted set. Write aggregate counts. | 009, 010 |
| **Eligibility re-evaluation (unblock) scanner** | `ff:idx:{p:N}:lane:<lane>:blocked:*` | 5–10s per partition | Re-evaluate block condition. If cleared (budget increased, quota reset, worker registered, lane unpaused), move execution back to eligible set. | 008, 009, 010 |
| **Execution/attempt timeout scanner** | `ff:idx:{p:N}:attempt_timeout`, `:execution_deadline` | 1–2s per partition | `ZRANGEBYSCORE -inf <now>`. Expire active executions that exceeded per-attempt timeout or total deadline. Distinct from lease expiry. | 001, 010 |
| **Generalized index reconciler** | `ff:idx:{p:N}:all_executions` | 30–60s per partition | SSCAN execution IDs. Verify correct index membership for current state. ZADD if missing. Safety net for OOM partial writes. | 010 |
| **Worker claim count reconciler** | `ff:worker:<wid>` + `ff:idx:{p:N}:worker:<wid>:leases` | 10–30s per worker | Sum SCARD of worker lease sets across partitions. Reset `current_claim_count` if drifted. | 009, 010 |
| **Flow retention scanner** | `ff:idx:{fp:N}:terminal_flows` | 60–120s per flow partition | `ZRANGEBYSCORE -inf <retention_cutoff>`. Verify all members purged (cross-partition). Cascade DEL flow core + members + edges + adjacency + events + summary. | 007, 010 |

---

## Part 2 — Function Inventory and Operation Flows

## 4. Function Inventory

All FlowFabric operations are deployed as a **single Valkey Function library** (`flowfabric`). Functions are loaded via `FUNCTION LOAD REPLACE` on engine startup and persist across Valkey restarts. Invocation uses `FCALL ff_<operation> <numkeys> KEYS... ARGS...` (not EVALSHA). Every function within a single invocation touches only keys sharing one hash tag (`{p:N}`, `{b:M}`, `{q:K}`, or `{fp:N}`).

**Library source layout:** The Lua source is split into 11 domain files for maintainability: `lua/helpers.lua` (shared helpers), `lua/version.lua` (ff_version), `lua/execution.lua` (create/claim/complete/fail/cancel/delay/reclaim/expire), `lua/lease.lua` (renew/mark_expired/revoke), `lua/suspension.lua` (suspend/resume/pending_wp/expire/close_wp), `lua/signal.lua` (deliver/buffer/claim_resumed), `lua/stream.lua` (append_frame), `lua/flow.lua` (create_flow/add_member/cancel_flow/stage_edge/apply_dep/resolve_dep/evaluate/promote/replay), `lua/budget.lua` (create_budget/report_usage/reset/unblock/block), `lua/quota.lua` (create_quota_policy/check_admission), `lua/scheduling.lua` (issue_grant/change_priority/update_progress/promote_delayed/issue_reclaim_grant). A `build.rs` step in `ff-script` concatenates them with the `#!lua name=flowfabric` preamble into a single blob for `FUNCTION LOAD REPLACE`. `helpers.lua` is always first (shared helpers must be defined before any function that uses them).

**Concatenated library structure:**
```lua
#!lua name=flowfabric

-- Shared helpers (available to all functions in the library)
local function ok(...)  return {1, "OK", ...} end
local function err(...) return {0, ...} end
local function ok_already_satisfied(...) return {1, "ALREADY_SATISFIED", ...} end
local function ok_duplicate(...) return {1, "DUPLICATE", ...} end
local function is_set(v) return v ~= nil and v ~= false and v ~= "" end
local function hgetall_to_table(flat)
  local t = {}
  for i = 1, #flat, 2 do t[flat[i]] = flat[i + 1] end
  return t
end
-- mark_expired, initialize_condition, evaluate_signal_against_condition,
-- is_condition_satisfied, write_condition_hash, map_reason_to_blocking,
-- unpack_policy — all defined once, shared by all functions.

-- Version function (Phase 0)
redis.register_function('ff_version', function(keys, args)
  return {1, "OK", "1.0.0"}
end)

-- Operation functions (Phases 1-6)
redis.register_function('ff_create_execution', function(keys, args) ... end)
redis.register_function('ff_claim_execution', function(keys, args) ... end)
-- ... (all functions registered in the library)
```

### 4.1 Execution Partition Functions (`{p:N}`)

| # | Function (`ff_*`) | RFC | Purpose | Class | Key Count | KEYS (all `{p:N}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 0 | `create_execution` | RFC-001 | Create execution core hash + payload + policy + tags, set initial state vector (all 7 dims), ZADD to eligible or delayed or blocked set, SET NX idempotency key, SADD all_executions (§1.1). If timeout_policy.deadline_at set: ZADD execution_deadline index. | A | 8 | exec_core, payload, policy, tags, eligible_or_delayed_zset, idem_key, execution_deadline_zset, all_executions |
| 1 | `claim_execution` | RFC-001 | Consume claim-grant, create new attempt + lease, transition runnable→active. Derives attempt_type from exec core `attempt_state` (pending_first→initial, pending_retry→retry, pending_replay→replay). Copies pending lineage fields from exec core to attempt, then clears them. ZADD attempt_timeout index (score=now+timeout_ms). | A | 14 | exec_core, claim_grant, eligible_zset, lease_expiry_zset, worker_leases, attempt_hash, attempt_usage, attempt_policy, attempts_zset, lease_current, lease_history, active_index, attempt_timeout_zset, execution_deadline_zset |
| 2 | `claim_resumed_execution` | RFC-001 | Consume claim-grant, resume existing attempt (interrupted→started), new lease, transition runnable→active. ZADD attempt_timeout (score=now+remaining_timeout). | A | 11 | exec_core, claim_grant, eligible_zset, lease_expiry_zset, worker_leases, existing_attempt_hash, lease_current, lease_history, active_index, attempt_timeout_zset, execution_deadline_zset |
| 3 | `complete_execution` | RFC-001 | Validate lease, end attempt, close stream, transition active→terminal(success). ZREM from attempt_timeout + execution_deadline. | A | 12 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, lease_current, lease_history, active_index, stream_meta, result_key, attempt_timeout_zset, execution_deadline_zset |
| 4 | `fail_execution` | RFC-001 | Validate lease, end attempt, close stream, decide retry. Does NOT create retry attempt — sets `pending_retry_attempt` + lineage on exec core; `claim_execution` creates the attempt. ZREM attempt_timeout. Terminal: ZREM execution_deadline. | A | 12 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, delayed_zset, lease_current, lease_history, active_index, stream_meta, attempt_timeout_zset, execution_deadline_zset |
| 5 | `acquire_lease` | RFC-003 | *See #1 (`claim_execution`). Same logical operation described from lease perspective.* | A | — | *See #1* |
| 6 | `renew_lease` | RFC-003 | Validate lease identity + epoch + expiry, extend expires_at, update lease_expiry index | A | 4 | exec_core, lease_current, lease_history, lease_expiry_zset |
| 7 | `complete_or_fail` | RFC-003 | *See #3/#4. Generic template described from lease perspective.* | A | — | *See #3/#4* |
| 8 | `revoke_lease` | RFC-003 | Validate target lease, set ownership_state=lease_revoked, record revocation, update indexes | A | 5 | exec_core, lease_current, lease_history, lease_expiry_zset, worker_leases |
| 9 | `reclaim_execution` | RFC-003 | Validate expired/revoked ownership. **Check `lease_reclaim_count < max_reclaim_count`** (default 100) — if exceeded, transition to terminal(failed, max_reclaims_exceeded) instead of creating new attempt. Otherwise: consume claim-grant, create new attempt + lease, increment epoch. | A | 16 | exec_core, old_attempt, new_attempt, attempts_zset, new_policy, new_usage, old_lease, new_lease, lease_history, lease_expiry_zset, old_worker_leases, new_worker_leases, claim_grant, old_stream_meta, active_index |
| 10 | `create_and_start_attempt` | RFC-002 | *See #1 (`claim_execution`). Same logical operation described from attempt perspective.* | A | — | *See #1* |
| 11 | `end_attempt_and_decide` | RFC-002 | *See #3/#4. Same logical operation described from attempt perspective. Includes stream close.* | A | — | *See #3/#4* |
| 12 | `interrupt_and_reclaim` | RFC-002 | *See #9 (`reclaim_execution`). Same logical operation described from attempt perspective. Includes stream close.* | A | — | *See #9* |
| 12a | `cancel_execution` | RFC-001 | Cancel from any non-terminal state. **Exec_core HSET (terminal:cancelled) is FIRST mutation** (§4.8b Rule 2). Active: validate lease or operator override, end attempt, close stream, clear lease. Runnable: defensive ZREM from ALL scheduling sets. Suspended: HSET exec_core terminal (first), end attempt (suspended→ended_cancelled), close stream (closed_at, closed_reason=attempt_cancelled), close waitpoint+wp_condition+suspension, ZREM suspended+suspension_timeout. All paths: ZREM from attempt_timeout + execution_deadline. **ZADD terminal_zset is UNCONDITIONAL.** Uses defensive ZREM from ALL scheduling sorted sets (not a single source_state_zset) to avoid TOCTOU race where execution moves between sets between caller's state read and Lua execution. | A | 21 | exec_core, attempt_hash, stream_meta, lease_current, lease_history, lease_expiry_zset, worker_leases, suspension_current, waitpoint_hash, wp_condition, suspension_timeout_zset, terminal_zset, attempt_timeout_zset, execution_deadline_zset, eligible_zset, delayed_zset, blocked_deps_zset, blocked_budget_zset, blocked_quota_zset, blocked_route_zset, blocked_operator_zset |
| 12b | `replay_execution` | RFC-001 | Validate terminal, set `pending_replay_attempt` + lineage on exec core (does NOT create attempt — `claim_execution` creates it with type=replay), transition terminal→runnable. **If `terminal_outcome = skipped` AND `flow_id` set:** reset dep edges (impossible→unsatisfied), recompute `deps:meta` counts, set `blocked_by_dependencies` instead of `eligible_now`, ZADD blocked:deps (engine dispatches cross-partition `resolve_dependency` post-script). **Otherwise:** ZADD eligible. Both paths: ZREM terminal. | A | 4+N | exec_core, terminal_zset, eligible_zset, lease_history (base 4). **If skipped flow member:** +blocked_deps_zset, +deps_meta, +deps_unresolved, +N×dep_edge_hash (N = number of dep edges to reset, from 0 to edge_count). Non-flow or non-skipped replays use only the base 4 keys. |
| 13 | `suspend_execution` | RFC-004 | Validate lease (incl. expiry + revocation checks matching complete_or_fail template), release ownership, create suspension + waitpoint (or activate pending), initialize wp_condition hash, ZADD suspended_zset, ZREM attempt_timeout, update execution state, update indexes | A | 16 | exec_core, attempt_record, lease_current, lease_history, lease_expiry_zset, worker_leases, suspension_current, waitpoint_hash, waitpoint_signals, suspension_timeout_zset, pending_wp_expiry_zset, active_index, suspended_zset, waitpoint_history, wp_condition, attempt_timeout_zset |
| 14 | `resume_execution` | RFC-004 | Validate suspension + waitpoint satisfied, close suspension + waitpoint, ZREM suspended_zset, transition suspended→runnable, update indexes | A | 8 | exec_core, suspension_current, waitpoint_hash, waitpoint_signals, suspension_timeout_zset, eligible_zset, delayed_zset, suspended_zset |
| 15 | `create_pending_waitpoint` | RFC-004 | Validate active lease, create pending waitpoint with short expiry, add to pending_wp_expiry index | A | 3 | exec_core, waitpoint_hash, pending_wp_expiry_zset |
| 16 | `expire_suspension` | RFC-004 | Validate suspension still active + timeout due, apply timeout_behavior. **Terminal paths (fail/cancel/expire):** exec_core FIRST (§4.8b Rule 2), end attempt, close stream, close waitpoint + wp_condition + suspension, ZREM suspended + suspension_timeout, ZADD terminal. **auto_resume:** close waitpoint + wp_condition + suspension, transition to runnable, ZREM suspended + suspension_timeout, ZADD eligible or delayed. **escalate:** mutate suspension reason_code. Overlap group D with #19. | A | 12 | exec_core, suspension_current, waitpoint_hash, wp_condition, attempt_hash, stream_meta, suspension_timeout_zset, suspended_zset, terminal_zset, eligible_zset, delayed_zset, lease_history |
| 17 | `deliver_signal` | RFC-005 | Validate target, **check signal count limit** (ZCARD exec_signals_zset vs max_signals_per_execution, default 10K — reject with `signal_limit_exceeded` if over), check idempotency, record signal, evaluate resume condition, optionally close waitpoint + suspension + transition suspended→runnable | A | 13 | exec_core, wp_condition, wp_signals_stream, exec_signals_zset, signal_hash, signal_payload, idem_key, waitpoint_hash, suspension_current, eligible_zset, suspended_zset, delayed_zset, suspension_timeout_zset |
| 18 | `buffer_signal_for_pending_waitpoint` | RFC-005 | Accept signal for pending waitpoint without evaluating resume conditions | A | 7 | exec_core, wp_condition, wp_signals_stream, exec_signals_zset, signal_hash, signal_payload, idem_key |
| 19 | `timeout_waitpoint` | RFC-005 | Generate synthetic timeout signal, apply timeout_behavior, close waitpoint + suspension, transition execution state. **Overlap group D with #16 (same script).** | A | 12 | *See #16 (`expire_suspension`). Same operation — implement as ONE script.* |
| 20 | `append_frame` | RFC-006 | Validate lease (lease_id + epoch + expiry), lazy-create stream metadata, XADD frame, XTRIM retention | B | 3 | exec_core, stream_key, stream_meta |
| 21 | `close_stream` | RFC-006 | Set closed_at + closed_reason on stream metadata (called within end_attempt scripts) | A | 1 | stream_meta |
| 22 | `apply_dependency_to_child` | RFC-007 | Create dep record, increment unsatisfied count. If runnable: set blocked_by_dependencies, ZREM eligible, ZADD blocked:dependencies | A | 6 | exec_core, deps_meta, unresolved_set, dep_hash, eligible_zset, blocked_deps_zset |
| 23 | `resolve_dependency` | RFC-007 | Satisfy or skip. Satisfaction: ZREM blocked:deps, set eligible_now, ZADD eligible. Skip: end attempt (if exists) + close stream, ZREM blocked:deps, set terminal=skipped, ZADD terminal | A | 9 | exec_core, deps_meta, unresolved_set, dep_hash, eligible_zset, terminal_zset, blocked_deps_zset, attempt_hash, stream_meta |
| 24 | `evaluate_flow_eligibility` | RFC-007 | Read-only check of execution + dependency state, return eligibility status | C | 2 | exec_core, deps_meta |
| 25 | `issue_claim_grant` | RFC-009 | Validate execution eligible, check no existing grant, write grant with TTL, remove from eligible set | A | 3 | exec_core, claim_grant_key, eligible_zset |
| 26 | `issue_reclaim_grant` | RFC-009 | Validate ownership_state in {expired_reclaimable, revoked}, write grant with TTL. No ZREM (exec not in eligible set) | A | 2 | exec_core, claim_grant_key |
| 27 | `promote_delayed` | RFC-001/009 | Verify runnable + not_eligible_until_time. All 7 dims: lifecycle=runnable (unchanged), ownership=unowned (unchanged), eligibility=eligible_now, blocking_reason=waiting_for_worker, blocking_detail='', terminal=none (unchanged), **attempt_state=PRESERVED** (do NOT overwrite — may be pending_retry_attempt from fail_execution or attempt_interrupted from delay_execution), public_state=waiting. ZREM delayed, ZADD eligible with composite score. | A | 3 | exec_core, delayed_zset, eligible_zset |
| 28 | `mark_lease_expired_if_due` | RFC-003 | Re-validate lease expiry on exec core, set ownership_state=lease_expired_reclaimable if confirmed. No-op if renewed/reclaimed | A | 4 | exec_core, lease_current, lease_expiry_zset, lease_history |
| 29a | `block_execution_for_admission` | RFC-009 | Parameterized block: set eligibility/blocking/public for budget/quota/route/lane denial, ZREM eligible, ZADD target blocked set. All 7 dims. | A | 3 | exec_core, eligible_zset, target_blocked_zset |
| 29b | `unblock_execution` | RFC-010 | Re-evaluate blocked execution, set eligible_now + waiting_for_worker + blocking_detail="" (clear per §4.8k), ZREM blocked set, ZADD eligible with composite priority score. All 7 dims. | A | 3 | exec_core, source_blocked_zset, eligible_zset |
| 29c | `expire_execution` | RFC-001 | Engine-initiated on attempt timeout or execution deadline. Handles **three lifecycle phases**: (1) `active`: exec_core FIRST (§4.8b Rule 2), close attempt + stream, release lease, ZREM from lease_expiry + worker_leases + active_index. (2) `runnable`: exec_core FIRST, defensive ZREM from ALL scheduling sets. (3) `suspended`: exec_core FIRST, close suspension + waitpoint + wp_condition, terminate attempt, close stream, ZREM from suspended + suspension_timeout. All paths: set terminal_outcome=expired, all 7 dims, blocking_detail='', ZREM from attempt_timeout + execution_deadline, ZADD terminal. Uses defensive ZREM from all scheduling sets (matching cancel_execution pattern). | A | 23 | exec_core, attempt_hash, lease_expiry_zset, worker_leases, terminal_zset, lease_current, lease_history, active_index, stream_meta, attempt_timeout_zset, execution_deadline_zset, suspended_zset, suspension_current, waitpoint_hash, wp_condition, suspension_timeout_zset, eligible_zset, delayed_zset, blocked_deps_zset, blocked_budget_zset, blocked_quota_zset, blocked_route_zset, blocked_operator_zset |
| 30 | `delay_execution` | RFC-001 | Worker delays its own active execution. Validate lease, release ownership. All 7 dims: lifecycle=runnable, ownership=unowned, eligibility=not_eligible_until_time, blocking_reason=waiting_for_delay, blocking_detail='delayed until \<delay_until\>', terminal=none, attempt_state=attempt_interrupted, public_state=delayed. Pause attempt (started→suspended). ZREM from active + lease_expiry + attempt_timeout. ZADD delayed (score=delay_until). Release lease + XADD lease_history. | A | 9 | exec_core, attempt_hash, lease_current, lease_history, lease_expiry_zset, worker_leases, active_index, delayed_zset, attempt_timeout_zset |
| 31 | `move_to_waiting_children` | RFC-001 | Worker blocks on child dependencies. Validate lease, release ownership, set blocked_by_dependencies + waiting_for_children, all 7 dims. ZREM from active + lease_expiry + attempt_timeout. ZADD blocked:dependencies. | A | 7 | exec_core, lease_current, lease_history, lease_expiry_zset, worker_leases, active_index, blocked_deps_zset |
| 32 | `change_priority` | RFC-001 | Update priority field + re-score in current scheduling sorted set. Validate runnable + eligible. ZREM old score, ZADD new composite score. | A | 2 | exec_core, eligible_zset |
| 33 | `update_progress` | RFC-001 | Update progress_pct + progress_message. Validate lease (lease_id + epoch). | B | 1 | exec_core |
| 34 | `report_usage_on_attempt` | RFC-001/008 | **Idempotent** via monotonic `usage_report_seq`. ARGV includes `usage_report_seq` (caller-supplied, monotonically increasing per attempt). Script checks `attempt_usage.last_usage_report_seq`: if `ARGV.seq <= last_seq` → return `ok_already_applied` (no-op). Otherwise: HINCRBY attempt usage counters + exec core usage totals, HSET `last_usage_report_seq = ARGV.seq`. Validate lease. Route to `{b:M}` budget check after return. Budget `report_usage_and_check` on `{b:M}` must also include the seq — if `ok_already_applied`, skip the budget call entirely. | B | 2 | exec_core, attempt_usage |
| 35 | `promote_blocked_to_eligible` | RFC-007 | Promote zero-dep flow member from blocked:dependencies to eligible. Validate runnable + blocked_by_dependencies + unsatisfied_required_count=0 + deps:unresolved empty. Set eligible_now + waiting_for_worker + blocking_detail="". **Preserve attempt_state** (same logic as resolve_dependency satisfaction path). ZREM blocked:deps, ZADD eligible with composite priority score. Called by control plane immediately after flow setup for root members with zero dependencies. | A | 5 | exec_core, blocked_deps_zset, eligible_zset, deps_meta, deps_unresolved |
| 36 | `close_waitpoint` | RFC-004 | Proactive close of pending or active waitpoint. Validate waitpoint exists and state is `pending` or `active` (not already closed/expired). HSET state=closed, closed_at=now, close_reason=ARGV.reason. If pending: ZREM from pending_wp_expiry. Does NOT change execution lifecycle state. Used by workers that created a pending waitpoint (SDK §10.3 step 7a) but decided not to suspend. | A | 3 | exec_core, waitpoint_hash, pending_wp_expiry_zset |
| — | `skip_execution` | RFC-001 | *Implemented inline in `resolve_dependency` (#23) impossible path. Not a standalone script.* | — | — | — |
| — | `release_lease` | RFC-001 | *Implemented inline in complete/fail/suspend/delay/move_to_waiting_children scripts. Not standalone.* | — | — | — |
| — | `recover_abandoned_execution` | RFC-001 | *Same as `reclaim_execution` (#9). See RFC-003.* | — | — | — |
| — | `record_result` | RFC-001 | *Inline in `complete_execution` (#3) — SET on result key. Standalone use for partial results: direct SET with lease validation (Class B, 2 keys).* | — | — | — |

### 4.2 Flow Partition Functions (`{fp:N}`)

| # | Function (`ff_*`) | RFC | Purpose | Class | Key Count | KEYS (all `{fp:N}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| 29 | `stage_dependency_edge` | RFC-007 | Validate membership + topology, check `expected_graph_revision` matches current (reject with `stale_graph_revision` on mismatch), create edge record, increment graph_revision, create mutation grant. ARGV must include `expected_graph_revision`. | A | 6 | flow_core, members_set, edge_hash, out_adj_set, in_adj_set, grant_hash |
| — | `create_flow` | RFC-007 | Create flow container. Idempotent (EXISTS → ok_already_satisfied). Sets graph_revision=0, node_count=0, edge_count=0, public_flow_state=open. | A | 2 | flow_core, members_set |
| — | `add_execution_to_flow` | RFC-007 | Add member to flow. Idempotent (SISMEMBER). Increments node_count + graph_revision. Does NOT set flow_id on exec_core (cross-partition). | A | 2 | flow_core, members_set |
| — | `cancel_flow` | RFC-007 | Cancel flow. Rejects if terminal. Returns member list for cross-partition cancel dispatch. Sets public_flow_state=cancelled. | A | 2 | flow_core, members_set |

### 4.3 Budget Partition Functions (`{b:M}`)

| # | Function (`ff_*`) | RFC | Purpose | Class | Key Count | KEYS (all `{b:M}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| — | `create_budget` | RFC-008 | Create budget policy with hard/soft limits on N dimensions. Idempotent. Variable ARGV for dimensions. Schedules periodic reset if reset_interval_ms > 0. | A | 4 | budget_def, budget_limits, budget_usage, budget_resets_zset |
| 30 | `report_usage_and_check` | RFC-008 | **Check-before-increment.** Read current usage, check hard limits. If any dimension would breach: return HARD_BREACH without incrementing (zero overshoot). If safe: HINCRBY all dimensions, check soft limits. Atomic Lua serialization on `{b:M}` guarantees no concurrent overshoot. | A | 3 | budget_usage, budget_limits, budget_def |
| 31 | `reset_budget` | RFC-008 | Zero all usage counters, record reset event, compute next_reset_at, re-score in reset index | A | 3 | budget_usage, budget_def, budget_resets_zset |

### 4.4 Quota Partition Functions (`{q:K}`)

| # | Function (`ff_*`) | RFC | Purpose | Class | Key Count | KEYS (all `{q:K}`) |
|---|-------------|-----|---------|-------|-----------|---------------------|
| — | `create_quota_policy` | RFC-008 | Create quota/rate-limit policy. Idempotent. Stores window_seconds, max_requests_per_window, active_concurrency_cap. Inits concurrency counter to 0. | A | 3 | quota_def, quota_window_zset, quota_concurrency_counter |
| 32 | `check_admission_and_record` | RFC-008 | **Idempotent.** Checks admitted guard key (`ff:quota:{q:K}:<policy_id>:admitted:<execution_id>`) — if exists, return `ALREADY_ADMITTED`. Otherwise: sliding-window ZREMRANGEBYSCORE + ZCARD rate check, INCR concurrency check, ZADD (member=execution_id alone, not execution_id:timestamp) + SET NX guard + INCR on admit. Guard TTL = window size. | A | 4 | window_zset, concurrency_counter, quota_def, admitted_guard_key |

### 4.5 Function Overlap Notes

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

| Partition | Function Count | Notes |
|-----------|---------------|-------|
| `{p:N}` execution | 30 | All claim/lease/attempt/suspension/signal/stream/promotion/cancel/replay functions. |
| `{fp:N}` flow | 4 | Flow lifecycle (create, add member, cancel) + edge staging. |
| `{b:M}` budget | 3 | Budget create, usage increment + breach check, reset. |
| `{q:K}` quota | 2 | Quota policy create, admission check + record. |
| **Total listed** | **39** | Including cross-reference entries for overlap groups. |
| **Unique `redis.register_function` calls** | **43** | After deduplicating overlap groups A-D + `ff_version`. All registered in one `flowfabric` library. Breakdown: execution.lua (9), lease.lua (3), scheduling.lua (5), suspension.lua (5), signal.lua (3), stream.lua (1), budget.lua (5), quota.lua (2), flow.lua (9), version.lua (1). |

### 4.8 Implementation Notes for Valkey Functions

These notes apply to ALL functions in the `flowfabric` library across all RFCs. Mandatory reading before implementation.

#### (pre) Valkey Functions deployment model

All FlowFabric Lua logic is deployed as a **single Valkey Function library** (`#!lua name=flowfabric`). This replaces the EVALSHA/SCRIPT LOAD pattern.

**Key differences from EVALSHA:**
- **No SHA caching.** Functions are registered by name, not by script hash.
- **No NOSCRIPT retry.** Functions persist across Valkey restarts and replicate to replicas automatically.
- **Shared helpers.** Library-local Lua functions (`is_set`, `err`, `ok`, `hgetall_to_table`, `mark_expired`, `initialize_condition`, `map_reason_to_blocking`, etc.) are defined once at the top of the library and available to all registered functions. This eliminates duplication and ensures consistent behavior.
- **Invocation:** `FCALL ff_<operation> <numkeys> KEYS... ARGS...` (not `EVALSHA <sha> <numkeys> ...`).
- **Loading:** `FUNCTION LOAD REPLACE <library_source>` on each Valkey primary at engine startup. In Valkey Cluster: route to all primaries. The library auto-replicates to replicas.
- **Version check:** `FCALL ff_version 0` → returns `{1, "OK", "<version>"}`. If version mismatch or ERR (function not found): `FUNCTION LOAD REPLACE`.

#### Library-Local Helpers

These are `local function` definitions in the library preamble, shared by all registered functions. They are NOT independently FCALL-able — they are internal implementation helpers.

**Return wrappers:**

| Helper | Signature | Used By |
|---|---|---|
| `ok(...)` | `return {1, "OK", ...}` | All functions |
| `err(...)` | `return {0, ...}` | All functions |
| `ok_already_satisfied(...)` | `return {1, "ALREADY_SATISFIED", ...}` | `ff_suspend_execution` |
| `ok_duplicate(sig_id)` | `return {1, "DUPLICATE", sig_id}` | `ff_deliver_signal` |

**Data access:**

| Helper | Purpose | Used By |
|---|---|---|
| `hgetall_to_table(flat)` | Converts HGETALL flat array to dict table | All functions reading hashes |
| `is_set(v)` | `v ~= nil and v ~= false and v ~= ""` — safe nil/empty check | All functions checking optional fields |

**Lease validation (most widely shared — prevents copy-paste drift):**

| Helper | Purpose | Used By |
|---|---|---|
| `validate_lease(core, argv, now_ms)` | Checks lifecycle=active, ownership not revoked, lease not expired, lease_id + epoch + attempt_id match. Returns error tuple or nil. | 7+ functions: complete, fail, suspend, delay, move_to_waiting_children, append_frame, report_usage |
| `mark_expired(keys, core, now_ms, maxlen)` | Sets `ownership_state = lease_expired_reclaimable`, `lease_expired_at`, XADD lease_history. Idempotent. | renew_lease, complete_or_fail (on expiry detection) |

**Suspension/signal condition evaluation (shared module):**

| Helper | Purpose | Used By |
|---|---|---|
| `map_reason_to_blocking(reason_code)` | Suspension reason → RFC-001 blocking_reason lookup table | `ff_suspend_execution` |
| `initialize_condition(json)` | Parse resume_condition_json → matcher table | `ff_suspend_execution`, `ff_deliver_signal` |
| `write_condition_hash(key, cond, now_ms)` | HSET condition state fields | `ff_suspend_execution`, `ff_deliver_signal` |
| `evaluate_signal_against_condition(cond, name, id)` | Match signal to matchers | `ff_suspend_execution` (buffered), `ff_deliver_signal` |
| `is_condition_satisfied(cond)` | Check mode (any/all/count) | `ff_suspend_execution`, `ff_deliver_signal` |
| `extract_field(fields, name)` | Valkey Stream entry array → named field | `ff_suspend_execution` (buffered signals) |

**Policy:**

| Helper | Purpose | Used By |
|---|---|---|
| `unpack_policy(json)` | `cjson.decode` → flat key-value pairs for HSET | `ff_claim_execution` (attempt policy) |
| `clear_lease_and_indexes(keys, core, reason, now_ms, maxlen)` | DEL lease_current, ZREM lease_expiry + worker_leases + active_index, clear lease fields on exec_core, XADD lease_history "released". Consolidates the ~15-line lease release block shared by 7 functions. | `ff_complete_execution`, `ff_fail_execution` (terminal), `ff_cancel_execution` (from active), `ff_expire_execution`, `ff_suspend_execution`, `ff_delay_execution`, `ff_move_to_waiting_children` |
| `defensive_zrem_all_indexes(keys, eid, except_key)` | ZREM execution_id from all scheduling + timeout indexes (eligible, delayed, active, suspended, terminal, 5 blocked sets, lease_expiry, suspension_timeout, attempt_timeout, execution_deadline, worker_leases) except `except_key`. ~14 ZREM/SREM calls. | `ff_cancel_execution`, `ff_expire_execution`, `ff_reconcile_execution_index`, cleanup cascade |

**16 library-local helpers total** (14 original + 2 added for lease release and defensive index cleanup).

**Note:** Budget/quota functions (`ff_report_usage_and_check`, `ff_check_admission_and_record`) use domain-specific return formats (`HARD_BREACH`, `ADMITTED`, etc.), not `ok()`/`err()`. They are engine-internal and do not share the worker-facing return convention (§4.9).

#### (a) HGETALL returns a flat array — use `hgetall_to_table`

Valkey `HGETALL` returns a flat array `{key1, val1, key2, val2, ...}`, NOT a Lua table with named fields. All RFC Lua pseudocode uses `core.lifecycle_phase` syntax which assumes a dict-style table. The shared `hgetall_to_table` helper handles this conversion:

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

#### (b) Valkey Functions are NOT transactional — partial writes persist on abort

Valkey Lua scripts execute each `redis.call()` independently. If a script aborts mid-execution (OOM, bug, or `redis.call()` error propagation), **all writes that already executed within the script persist**. There is no rollback.

Example: if `HSET exec_core lifecycle_phase=terminal` succeeds but the subsequent `ZADD terminal_zset` fails (OOM), the execution is terminal but not indexed. The generalized index reconciler (§6.17) catches this, but implementers must understand the risk.

**Defensive write ordering — two rules that apply in different contexts:**

**Rule 1 — Index writes before state for simple transitions:** Where possible, perform index writes (ZADD, ZREM, SADD) BEFORE state mutations (HSET exec_core). If an index write OOMs, the state hasn't changed — the execution remains in its previous consistent state. This applies to transitions like complete, fail, and claim where the index update and state update are on the same partition and the only sub-objects are indexes.

**Rule 2 — exec_core FIRST for transitions that create/close sub-objects:** Scripts that transition exec_core AND create or close sub-objects (suspension records, waitpoint records, attempt records) MUST write exec_core state FIRST, before creating or closing sub-objects. This ensures partial writes leave exec_core in the target lifecycle state, which reconcilers can detect and resolve. The reverse (sub-objects mutated but exec_core still in old state) creates zombie states that no scanner checks for. Applies to: `suspend_execution` (exec_core=suspended BEFORE suspension_current creation), `resume_execution` (exec_core=runnable BEFORE closing suspension/waitpoint), `cancel_suspension` (exec_core=terminal BEFORE closing sub-objects).

#### (b2) All exec_core mutations MUST go through Valkey Functions

No raw `HSET` calls from application code. Every state transition is an atomic Lua script that validates preconditions, updates the state vector, and maintains indexes in one call. A raw `HSET` that skips validation can violate invariants (e.g., setting `lifecycle_phase = active` without creating a lease).

#### (c) Empty string means "cleared" — use is_set() helper for checks

Lua scripts use `""` (empty string) to represent cleared optional fields. Valkey hashes cannot store nil — `HSET key field ""` stores an empty string. A field that was never HSET'd does not exist in the hash; `HGETALL` omits it (resulting in `nil` in the Lua table), and `HMGET` returns `false` for it.

**Use `is_set()` for all "does this field have a value?" checks:**

```lua
local function is_set(v)
  return v ~= nil and v ~= false and v ~= ""
end
```

The check `field ~= ""` is WRONG for fields that may not exist — `nil ~= ""` and `false ~= ""` are both `true`, which incorrectly treats a never-set field as having a value. This matters for optional fields like `lease_revoked_at`, `lease_expired_at`, `cancellation_reason`, etc.

#### (c2) KEYS and ARGV naming convention

All pseudocode uses **named KEYS** (`KEYS.exec_core`, `KEYS.eligible_zset`) and **named ARGV** (`ARGV.lease_id`, `ARGV.execution_id`) for readability. In Valkey Lua, both are positional arrays (`KEYS[1]`, `ARGV[1]`). The mapping from named references to positional indices is defined by the KEYS/ARGV comment at the top of each script. Implementers must map names to positions during implementation. The ordering is implementation-defined — the comment at the top of each script is the authoritative mapping.

#### (d) Priority score is IEEE 754 double — safe up to ~priority 9000

The composite score `-(priority * 1_000_000_000_000) + created_at_ms` is stored as a Valkey sorted set score (IEEE 754 double, 53-bit mantissa). With `created_at_ms` around 1.7 × 10^12 (current epoch ms), the score magnitude is `priority * 10^12 + 1.7 * 10^12`. IEEE 754 doubles lose integer precision above 2^53 = 9.0 × 10^15. Safe priority range: `|priority| <= ~9000` before FIFO tiebreaking becomes unreliable. V1 recommended range `[-1000, 1000]` is safe with wide margin.

#### (e) Dependency resolution trigger: engine dispatches after ANY terminal transition

Terminal-transition Lua scripts do NOT call `resolve_dependency` inline — they may be on different partitions. Instead, every terminal-transition script returns a flag or event indicating the execution has a `flow_id`. The engine layer (outside Lua) then:
1. Reads outgoing edges from `{fp:N}` flow partition.
2. **Re-reads the parent execution's `terminal_outcome` from `{p:N}` immediately before each `resolve_dependency` call.** If the parent is no longer terminal (e.g., operator replayed it between the terminal event and this dispatch), SKIP the dispatch for that edge. This prevents stale edge resolution — a replayed parent should not satisfy downstream dependencies using its old terminal outcome.
3. For each downstream child where the parent is still terminal, calls `ff_resolve_dependency` on the child's `{p:N}` partition with the parent's current `terminal_outcome`.

**This dispatch applies to ALL terminal transitions, not just completion:**

| Terminal Script | Upstream Outcome | Downstream Effect |
|---|---|---|
| `complete_execution` | `success` | Edge satisfied → may promote to eligible |
| `fail_execution` (terminal path) | `failed` | Edge impossible → downstream skipped |
| `cancel_execution` | `cancelled` | Edge impossible → downstream skipped |
| `expire_execution` | `expired` | Edge impossible → downstream skipped |
| `resolve_dependency` (skip path) | `skipped` | Edge impossible → downstream of the skipped node also skipped (cascade) |

Without dispatch on non-completion terminals, skip cascade propagation through a chain of depth N takes N × reconciler_interval (10-30s each), producing 30-90s+ delays for a 3-4 node chain. With dispatch, propagation is immediate (milliseconds per hop).

**Consistency window:** Between the terminal script returning and the engine re-reading in step 2, the parent may be replayed. Step 2's re-read closes this window. Without it, a stale dispatch satisfies a child's dependency using an outcome that no longer exists — the child runs with the old result while the parent re-executes.

**replay_execution interaction:** `replay_execution` already resets `impossible → unsatisfied` edges for skipped children (round 21 fix). A future enhancement should also reset `satisfied → unsatisfied` edges for children that haven't yet been claimed, enabling full re-evaluation of the dependency graph on parent replay. For v1, the step 2 re-read guard is sufficient — it prevents new stale resolutions. Already-satisfied edges from prior completions remain (documented in RFC-007 Open Questions §4).

This cross-partition dispatch is documented in §3.2 (Flow Dependency Resolution). If the engine crashes between any terminal script returning and the dependency resolution dispatch, the dependency resolution reconciler (§6.14) catches the gap. The reconciler is a **safety net**, not the primary propagation mechanism.

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

#### (i) Partition scanning strategy for candidate selection

The scheduler must scan eligible sorted sets across all partitions to find a candidate. With 256 execution partitions and 10 lanes, a naive scan requires 2,560 ZRANGEBYSCORE calls per scheduling cycle.

**V1 approach:** Pipeline all partition scans for the selected lane. Valkey pipeline round-trip for 256 ZRANGEBYSCORE commands is ~1-2ms. At 1000 claims/sec this is 1-2s of pipeline time — acceptable for 2-3 scheduler instances sharing the load.

**Future optimization:** Maintain a per-lane partition bitmap tracking which partitions have non-empty eligible sets. Update on ZADD/ZREM. Skip empty partitions (reduces calls by 90%+ under typical load). Alternatively, random partition sampling for very high partition counts.

#### (j) Quota policy caching

Quota and budget policy lookups (which policies attach to a lane or tenant) require cross-partition reads to `{q:K}` and `{b:M}`. These policies change rarely.

**V1 approach:** Scheduler caches lane-attached quota policies and budget summaries at startup. Refreshes every 60s. Only the actual `check_admission_and_record` Lua call (per-candidate) needs to be real-time — it reads the sliding window and concurrency counter, which are current.

#### (k) blocking_detail MUST accompany blocking_reason

Every HSET that changes `blocking_reason` MUST also set `blocking_detail` with a specific, human-readable explanation. When `blocking_reason = none`, set `blocking_detail = ""`.

| blocking_reason | blocking_detail example |
|---|---|
| `waiting_for_budget` | `"budget budget-abc123: total_cost 48M/50M (96%)"` |
| `waiting_for_quota` | `"quota quota-xyz: tokens_per_minute 95K/100K, resets in 8s"` |
| `waiting_for_children` | `"3 of 5 deps unresolved: [edge_A, edge_B, edge_C]"` |
| `waiting_for_capable_worker` | `"requires gpu=true, no registered worker has it"` |
| `waiting_for_locality_match` | `"requires region=us-east-1, nearest worker in eu-west-1"` |
| `waiting_for_retry_backoff` | `"retry backoff until 2026-04-14T10:30:00Z (attempt 3 of 5)"` |
| `waiting_for_resume_delay` | `"resume delay 5000ms, eligible at 2026-04-14T10:30:05Z"` |
| `paused_by_operator` | `"operator jane@example.com placed hold at 2026-04-14T10:25:00Z"` |
| `paused_by_policy` | `"lane inference-fast is in draining state"` |
| `none` | `""` |

#### (l) Helper Function Reference

All Lua pseudocode calls these helpers. Implementers must provide them.

**Return wrappers** (§4.9 defines format):

| Helper | Implementation | Complexity |
|---|---|---|
| `err(name)` | `return {0, name}` | Trivial |
| `ok(values...)` | `return {1, "OK", ...}` | Trivial |
| `ok_already_satisfied(...)` | `return {1, "ALREADY_SATISFIED", ...}` | Trivial |
| `ok_duplicate(sig_id)` | `return {1, "DUPLICATE", sig_id}` | Trivial |

**Validation** (inline):

| Helper | Source | Complexity |
|---|---|---|
| `assert_execution_exists(core)` | Check core not nil/empty | Trivial |
| `assert_flow_membership(core, fid)` | RFC-007 F2: `core.flow_id == fid` | Trivial |
| `assert_active_suspension(susp)` | RFC-004: suspension_id set, closed_at empty | Trivial |
| `assert_waitpoint_belongs(wp, eid, sid, wid)` | RFC-004: match execution+suspension+waitpoint | Trivial |
| `validate_claim_grant(grant, wid, wiid, lane, cap)` | RFC-003 §Acquisition: fields match caller | Trivial |
| `validate_pending_waitpoint(wp, eid, idx, key, now)` | RFC-004 §Pending: state=pending, not expired | Moderate |

**Condition evaluation** (shared module — implement together):

| Helper | Source | Complexity |
|---|---|---|
| `initialize_condition(json)` | RFC-004 §Resume Condition Model: parse → matcher table | Moderate |
| `write_condition_hash(key, cond, ts)` | RFC-005 §12.1: HSET condition fields | Moderate |
| `evaluate_signal_against_condition(cond, name, id)` | RFC-005 §8.3: match signal to matchers | Moderate |
| `is_condition_satisfied(cond)` | RFC-005 §8.3: check mode (any/all/count) | Moderate |
| `extract_field(fields, name)` | Valkey Stream entry array → named field | Trivial |

**Mapping/transform:**

| Helper | Source | Complexity |
|---|---|---|
| `map_reason_to_blocking(reason)` | RFC-004 §Suspension Reason Categories: lookup table | Trivial |
| `unpack_policy(json)` | RFC-002 §AttemptPolicySnapshot: JSON → HSET pairs | Moderate |

**State mutation and index cleanup:**

| Helper | Source | Complexity |
|---|---|---|
| `apply_attempt_outcome(key, outcome, ts, result)` | RFC-002 §Lifecycle States | Moderate |
| `apply_execution_transition(key, outcome, ts, fields)` | RFC-001 §3 transition rules | Moderate |
| `clear_current_lease_fields(key)` | RFC-001 §9.2 ownership fields → all `""` | Moderate |
| `clear_lease_and_indexes(keys, core, reason, now_ms, maxlen)` | RFC-003 lease release + RFC-001 index cleanup. 7 consumers. | Moderate |
| `defensive_zrem_all_indexes(keys, eid, except_key)` | RFC-010 §9.3 step 12 pattern. 3+ consumers. | Moderate |

#### (m) Forward compatibility — or-default pattern for new fields

All field reads in Lua must use the `or` default pattern: `core.field or "default"`. This allows new fields to be added to exec_core (or any hash) in v2+ without migrating existing records. Valkey hashes are schema-less — HGET on a nonexistent field returns nil. Existing v1 records missing the new field simply get the default value. No migration, no backfill, no downtime.

#### (n) Shared helpers eliminate consolidation concern

The Valkey Functions library model (§4.8 pre) solves the v1 consolidation problem natively. The `validate_lease` shared helper (see Library-Local Helpers above) is defined once and used by all functions that need lease validation (complete, fail, suspend, delay, move_to_waiting_children, append_frame, report_usage). No copy-paste of the lease validation preamble. Similarly, `mark_expired`, condition evaluation helpers, and return wrappers are defined once. This is the primary benefit of the single-library model over separate EVALSHA scripts.

### 4.9 Return Convention

All functions use the shared `err()` and `ok()` helpers defined in the library preamble. Concrete return format:

**Success:** `return {1, "OK", ...values}`
**Failure:** `return {0, "ERROR_NAME", ...context}`

Client SDK parses element [1]: `1` = success, `0` = failure.

**Do NOT use `redis.error_reply()` for Class A scripts** — it prevents returning structured context. Class B scripts MAY use `redis.error_reply()` for errors. **Exception:** `append_frame` returns the raw XADD entry ID string on success (e.g., `"1713100800150-0"`) — the sole exception to the `{1,"OK",...}` convention. The SDK handles this non-table return as a special case.

#### Success Variants (element [2])

Element [2] of success returns distinguishes success types. SDK MUST branch on element [2]:

| Element [2] | Meaning | Scripts |
|---|---|---|
| `"OK"` | Normal success. Values at element [3]+. | All (default) |
| `"ALREADY_SATISFIED"` | Suspension skipped — buffered signals satisfied condition. Lease still held. | `suspend_execution` |
| `"DUPLICATE"` | Signal deduplicated. Element [3] = existing signal_id. | `deliver_signal` |
| `"ALREADY_APPLIED"` | Idempotent no-op — usage seq already processed. | `report_usage_on_attempt` |

#### Sub-Status Strings (element [3] within OK)

Some scripts return a sub-status at element [3] for multiple success outcomes:

| Script | Element [3] | SDK Action |
|---|---|---|
| `fail_execution` | `"retry_scheduled"` / `"terminal_failed"` | Distinguish retry vs permanent failure |
| `resolve_dependency` | `"satisfied"` / `"impossible"` / `"already_resolved"` | Edge resolution outcome |
| `block_execution_for_admission` | `"blocked"` / `"not_runnable"` / `"already_blocked"` / `"terminal"` | Block outcome |

| Pseudocode | Concrete Return |
|---|---|
| `return err("stale_lease")` | `return {0, "stale_lease"}` |
| `return err("execution_not_active", outcome, epoch)` | `return {0, "execution_not_active", outcome, epoch}` |
| `return ok()` | `return {1, "OK"}` |
| `return ok(lease_id, epoch, expires_at)` | `return {1, "OK", lease_id, epoch, expires_at}` |
| `return ok("retry_scheduled", delay)` | `return {1, "OK", "retry_scheduled", delay}` |
| `return ok_duplicate(signal_id)` | `return {1, "DUPLICATE", signal_id}` |
| `return ok_already_satisfied(sid, wpid, wpkey)` | `return {1, "ALREADY_SATISFIED", sid, wpid, wpkey}` |

**Shared helpers (defined once in library preamble, available to all functions):**

```lua
local function ok(...)  return {1, "OK", ...} end
local function err(...) return {0, ...} end
local function ok_already_satisfied(...) return {1, "ALREADY_SATISFIED", ...} end
local function ok_duplicate(...) return {1, "DUPLICATE", ...} end
```

#### Engine-Internal Script Returns (RFC-008)

RFC-008 budget/quota scripts use **domain-specific return formats** called by the scheduler/engine, not the worker SDK. The scheduler translates returns to engine actions:

| Lua Return | Scheduler Action |
|---|---|
| `{"OK"}` | Continue (budget accepted). |
| `{"HARD_BREACH", dim, action}` | Apply enforcement on `{p:N}`. |
| `{"SOFT_BREACH", dim, action}` | Log warning, emit metric. |
| `{"ADMITTED"}` | Proceed to claim-grant. |
| `{"ALREADY_ADMITTED"}` | Proceed (idempotent). |
| `{"RATE_EXCEEDED", retry_ms}` | Block execution or return empty. |
| `{"CONCURRENCY_EXCEEDED"}` | Return empty to worker. |

Workers never see these. SDK parses only `{1,...}/{0,...}` from worker-facing scripts.

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
  │                │ (scope-level,    │             │              │
  │                │  fast pre-check) │             │              │
  │                │ ff_check_admis-  │             │              │
  │                │ sion_and_record  │             │              │
  │                │─────────────────►│             │              │
  │                │ (ADMITTED/DENIED)│             │              │
  │                │◄─────────────────│             │              │
  │                │                  │             │              │
  │                │ 2. SELECT        │             │              │
  │                │ CANDIDATE        │             │              │
  │                │ (fairness,       │             │              │
  │                │  priority,       │             │              │
  │                │  capability      │             │              │
  │                │  match)          │             │              │
  │                │                  │             │              │
  │                │ 3. BUDGET CHECK  │             │              │
  │                │ PER-CANDIDATE    │             │              │
  │                │ (advisory read   │             │              │
  │                │  of attached     │             │              │
  │                │  budgets — no    │             │              │
  │                │  mutation)       │             │              │
  │                │────────────────────────────────►│              │
  │                │                  │  (OK/BREACH)│              │
  │                │◄───────────────────────────────│              │
  │                │                  │             │              │
  │                │ [IF BUDGET DENIED]:            │              │
  │                │ 3a. block_execution_for_       │              │
  │                │     admission on THIS          │              │
  │                │     candidate's {p:N}          │              │
  │                │──────────────────────────────────────────────►│
  │                │◄─────────────────────────────────────────────│
  │                │ 3b. GOTO step 2 (next          │              │
  │                │     candidate). If no more     │              │
  │                │     candidates: return empty.  │              │
  │                │                  │             │              │
  │                │ [IF BUDGET OK]:  │             │              │
  │                │ 4. ISSUE GRANT   │             │              │
  │                │ ff_issue_claim_  │             │              │
  │                │ grant            │             │              │
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
  │ ff_claim_execution                │             │              │
  │ (or ff_claim_resumed_execution    │             │              │
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

  Lua scripts: check_admission_and_record → [select candidate] →
               budget read (per-candidate) → issue_claim_grant →
               claim_execution
  Partitions: {q:K} → [scheduler] → {b:M} (read) → {p:N} → {p:N}
              (4 sequential Lua calls, 3 potentially different shards)
  Budget denial loop: steps 2-3a may repeat for multiple candidates
  until one passes or no candidates remain.
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
  │  ff_complete_execution                │
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
  │  ff_fail_execution                     │
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
  │ ff_create_wp                          │
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
  │  ff_suspend_execution                 │
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
  │  ff_deliver_signal                     │
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
  │   ff_claim_resumed_execution)        │

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
  │                       │     ff_grant —       │
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
  │ ff_reclaim_execution  │                     │
  │ (or interrupt_and_    │                     │
  │  ff_reclaim)          │                     │
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
  │                   │    ff_stage_dependency_edge    │               │
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
  │                   │    ff_child                    │               │
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
  │                   │    (ff_complete_execution)    │               │
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
  │                   │    ff_resolve_dependency      │               │
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
  │  ff_cancel_execution                  │
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
  │  ff_replay_execution                  │
  │  ┌────────────────────────────────────┤
  │  │ 1. HGETALL exec_core              │
  │  │ 2. Validate: lifecycle=terminal    │
  │  │ 3. Enforce replay_count <          │
  │  │    max_replay_count                │
  │  │ 4. Store lineage on exec core:     │
  │  │    pending_replay_reason,          │
  │  │    pending_replay_requested_by,    │
  │  │    pending_previous_attempt_index  │
  │  │    (claim_execution creates the    │
  │  │     actual attempt record)         │
  │  │ 5. HSET exec_core: ALL 7 dims     │
  │  │    lifecycle=runnable              │
  │  │    terminal_outcome=none           │
  │  │    attempt_state=pending_replay    │
  │  │    replay_count++                  │
  │  │                                    │
  │  │ [IF terminal_outcome was skipped   │
  │  │  AND flow_id is set]:              │
  │  │ 5a. Reset dep edges:              │
  │  │     For each dep:<edge_id> with    │
  │  │     state=impossible:              │
  │  │     HSET state=unsatisfied         │
  │  │     SADD deps:unresolved           │
  │  │ 5b. HSET deps:meta:               │
  │  │     impossible_required_count=0    │
  │  │     recompute unsatisfied count    │
  │  │ 5c. Set eligibility=              │
  │  │     blocked_by_dependencies        │
  │  │     blocking_reason=               │
  │  │       waiting_for_children         │
  │  │     public_state=waiting_children  │
  │  │ 5d. ZREM terminal_zset            │
  │  │ 5e. ZADD blocked:dependencies     │
  │  │     (NOT eligible — deps need      │
  │  │      cross-partition resolution)   │
  │  │                                    │
  │  │ [ELSE — normal replay]:            │
  │  │ 5f. public_state=waiting           │
  │  │ 6. ZREM terminal_zset             │
  │  │ 7. ZADD eligible_zset (composite  │
  │  │    priority score)                 │
  │  └────────────────────────────────────┤
  │◄──────────────────────────────────────│
  │  ok(new_attempt_id, attempt_index)    │

  Lua scripts: replay_execution (1 script)
  Partitions: {p:N} only
  Post-script (skipped replay only): engine dispatches
    resolve_dependency per reset edge (cross-partition
    reads to upstream {p:N'} partitions, same pattern
    as §3.2). Dependency resolution reconciler (§6.14)
    is the safety net.
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
  │  ff_renew_lease                        │
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
| **Per-item action** | `FCALL ff_mark_lease_expired_if_due` on the execution core. Re-validates that the lease is actually expired (guards against clock skew or recent renewal). If confirmed, sets `ownership_state = lease_expired_reclaimable`. |
| **Failure mode** | Safe to crash. On restart, re-scans from `-inf`. Items already processed are idempotent (function re-validates). |
| **Idempotency** | The Lua script checks current lease state before acting. If already expired/reclaimed, it's a no-op. |
| **Batch size** | 50-100 per scan cycle. Larger batches risk Lua script latency spikes. |

**Reclaim discovery:** After `mark_lease_expired_if_due` transitions an execution to `lease_expired_reclaimable`, the scheduler must discover it for reclaim. Two approaches (implementation chooses one):

**(a) Scanner-notifies-scheduler (recommended):** The lease expiry scanner, after marking an execution as reclaimable, notifies the scheduler directly (e.g., adds the execution_id to an in-memory queue if scanner and scheduler share a process, or XADDs to a `ff:sched:reclaim_candidates` stream). The scheduler then runs capability matching → `issue_reclaim_grant` → worker calls `reclaim_execution`.

**(b) Scheduler polls lease_expiry index:** The scheduler periodically scans `ff:idx:{p:N}:lease_expiry` for entries with `score < now_ms`. For each, reads exec core to check `ownership_state = lease_expired_reclaimable`. If confirmed, proceeds with reclaim grant. This is simpler but duplicates the scanner's work.

Both approaches converge: the execution is reclaimed within `scanner_frequency + scheduler_cycle` (worst case ~3-4s). The lease_expiry ZSET entry is NOT removed by `mark_lease_expired_if_due` — it stays until `reclaim_execution` or `cancel_execution` removes it. This means both approaches can safely discover the execution from the same index.

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
| **Per-item action** | Run `FCALL ff_expire_suspension`. The script re-validates that the execution is still suspended and the timeout is still due (guards against concurrent resume). Applies the configured `timeout_behavior` (fail/cancel/expire/auto_resume/escalate) atomically. Sets all 7 state vector dimensions. |
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
| **Per-item action** | Read `ff:attempt:{p:N}:{execution_id}:{attempt_index}:usage` for each active execution. Sum dimensions. Compare with `ff:budget:{b:M}:{budget_id}:usage`. **Only correct UPWARD** — for each dimension, set counter to `max(current_counter, computed_sum)`. Never correct downward. Upward-only is conservative (blocks sooner) and safe. **Stale reference cleanup:** If the attempt usage key returns nil/empty (execution purged by the terminal retention scanner on `{p:N}`), SREM the `execution_id` from `ff:budget:{b:M}:{budget_id}:executions`. This prevents unbounded growth of the reverse index with stale execution references. The cleanup is incremental — each reconciler run cleans one batch. |
| **Failure mode** | Safe to crash. Drift is bounded (one concurrent request per enforcement point). Reconciliation on next cycle. Stale SREMs are idempotent. |
| **Idempotency** | Reads actual state and applies max(). Running twice is safe — max is idempotent. SREM on non-member is a no-op. |
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

**The projector IS the flow completion evaluator.** The projector reads **authoritative** member execution states via cross-partition HMGET calls to each member's `{p:N}` partition — not from the summary hash. After aggregating counts from these authoritative reads, it:
1. Writes updated counts to `ff:flow:{fp:N}:<flow_id>:summary`.
2. Evaluates the flow's `completion_policy` (from `ff:flow:{fp:N}:<flow_id>:core`) against the aggregated counts.
3. If the completion policy is satisfied: HSET `public_flow_state` on both the summary hash AND the flow core hash (`ff:flow:{fp:N}:<flow_id>:core`).
4. Similarly evaluates `failure_policy` and `cancellation_propagation` state.

No separate process evaluates flow completion. The projector is the **sole authority** for `public_flow_state` transitions. This is consistent with §4.8(f) — the projector already reads authoritative states, so it can evaluate policies directly without a second pass.

**Latency implication:** Flow completion detection latency = projector cycle time (event-driven: sub-second on state change events; periodic fallback: 10-30s). For time-sensitive flows, ensure the event-driven path fires on every member terminal transition.

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
| **Per-item action** | `FCALL ff_promote_delayed`: verify execution is still `runnable` + `not_eligible_until_time`. Set `eligibility_state = eligible_now`, `blocking_reason = waiting_for_worker`, `blocking_detail = ""`, `public_state = waiting`. ZREM from delayed, ZADD to eligible with priority score. |
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
| **Per-item action** | `FCALL ff_reset_budget`: zero all usage counters, record reset event, compute `next_reset_at`, re-score in index. |
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
| **Operation** | For each execution in the blocked set: read `ff:exec:{p:N}:<execution_id>:deps:meta` to get `flow_id` and unsatisfied count. For each unresolved edge in `ff:exec:{p:N}:<execution_id>:deps:unresolved`: read the upstream execution's terminal state from its `{p:N}` partition (cross-partition read). If upstream is terminal, call `FCALL ff_resolve_dependency` on this child's partition. |
| **Per-item action** | `FCALL ff_resolve_dependency` per stale unresolved edge. Idempotent — already-resolved edges return `already_resolved`. |
| **Failure mode** | Safe to crash. Unresolved dependencies remain in the blocked set and are picked up on next scan. |
| **Idempotency** | `resolve_dependency` checks edge state before acting. Satisfied or impossible edges are skipped. |
| **Batch size** | 20-50 per cycle. Each item may require 1-3 cross-partition reads (upstream execution states). |

**Zero-dep promotion check:** Additionally, for each execution in the blocked set: if `unsatisfied_required_count == 0` AND `deps:unresolved` is empty (SCARD == 0) AND `eligibility_state == blocked_by_dependencies` → promote to `eligible_now`. Atomic Lua: HSET eligibility=eligible_now + blocking_reason=waiting_for_worker + public_state=waiting + blocking_detail="". ZREM from blocked:dependencies. ZADD to eligible with composite priority score. This catches: (a) engine crash before dependencies were applied (child created blocked but deps never staged), (b) dependency-free flow children not explicitly promoted by the caller, (c) any scenario where all deps resolved but the promotion HSET/ZADD was lost (OOM).

**When this reconciler fires:** Normally, dependency resolution is dispatched by the engine within milliseconds of parent completion. The reconciler is a safety net for: engine crash between parent complete and child resolve dispatch, network partition between engine and Valkey during dispatch, failure reading outgoing edges from the flow partition, or zero-dep stuck executions.

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

**Cross-partition state caching (MANDATORY):** Per scan cycle, the unblock scanner MUST cache cross-partition state to avoid redundant reads. Without caching, 50K budget-blocked executions sharing the same budget trigger 50K HMGET calls to the same `{b:M}` hash per cycle. Cache per cycle:
- **Budget:** `Map<budget_id, has_headroom>` — first execution triggers read, rest reuse.
- **Quota:** `Map<quota_policy_id, can_admit>` — same pattern.
- **Workers:** `Map<capability_hash, capable_exists>` — read registrations once.
- **Lanes:** `Map<lane_id, state>` — read config once.
Cache invalidated at cycle end. Staleness within one cycle (5-10s) is acceptable.

**Critical: `blocking_reason` field check.** The `blocked:operator` set contains both lane-paused (`paused_by_policy`) and explicit operator-held (`paused_by_operator`) executions. The scanner must read each execution's `blocking_reason` before unblocking. Each block type has an expected reason:

| Scanner | Expected `blocking_reason` | Skip if |
|---|---|---|
| Budget | `waiting_for_budget` | Any other |
| Quota | `waiting_for_quota` | Any other |
| Route | `waiting_for_capable_worker` or `waiting_for_locality_match` | Any other |
| Lane (`blocked:operator`) | `paused_by_policy` | `paused_by_operator`, `paused_by_flow_cancel` |

**`paused_by_flow_cancel` MUST be skipped.** This blocking reason is set by `cancel_flow` with `let_active_finish` policy (RFC-007). Only `cancel_flow` clears it — when active members drain to zero, it cancels the blocked members. The unblock scanner must NOT unblock these executions, even if the lane is open. Without this check, the scanner creates a race: it unblocks flow-cancelled executions (lane is open → unblock), allowing them to be claimed and run, violating the cancellation intent.

| Property | Value |
|---|---|
| **Per-item action** | Read exec core `blocking_reason`. If mismatch with expected: skip. If match: re-evaluate external condition. If cleared: run `ff_unblock_execution` — verify still blocked, set `eligibility_state = eligible_now`, `blocking_reason = waiting_for_worker`, `blocking_detail = ""` (clear per §4.8k), `public_state = waiting`, ZREM blocked set, ZADD eligible set with composite priority score. All 7 dims. |
| **Failure mode** | Safe to crash. Blocked executions remain blocked until next scan. |
| **Idempotency** | `ff_unblock_execution` checks `eligibility_state` before acting. Already-eligible or already-claimed executions are skipped. |
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
| **Per-item action** | Run `FCALL ff_expire_execution`: validate execution is not already terminal. Handles three lifecycle phases: **(1) active** — close attempt (ended_failure with failure_reason=attempt_timeout or execution_deadline), close stream, release lease, ZREM from lease_expiry + worker_leases + active_index. **(2) runnable** — ZREM from current scheduling index (eligible/delayed/blocked set). If attempt exists (pending_retry/pending_replay), no attempt state change (attempt was never started). **(3) suspended** — close waitpoint (state=closed, close_reason=expired), close suspension (close_reason=timed_out_expire), terminate attempt (ended_failure), ZREM from suspended + suspension_timeout. **All paths:** set `terminal_outcome = expired`, `attempt_state = attempt_terminal` (or `none` if no attempt), `failure_reason = "attempt_timeout"` or `"execution_deadline"`, `blocking_detail = ""`. ZREM from attempt_timeout and execution_deadline. ZADD terminal. All 7 state vector dimensions. |
| **Failure mode** | Safe to crash. Overdue entries remain in the index. Picked up on next scan. |
| **Idempotency** | Script checks `lifecycle_phase != terminal` before acting. Already-terminal executions are no-ops. Handles active, runnable, and suspended phases. |
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

**CRITICAL: Per-item fix MUST be an atomic Valkey Function.** Between the SCAN discovery and the fix, the execution's state may change (e.g., claimed by a worker). The function re-reads exec_core via HMGET and validates state before any writes. Without atomicity, the reconciler could create phantom index entries.

**Pseudocode: `ff_reconcile_execution_index`**

```lua
-- KEYS: exec_core, eligible_zset, delayed_zset, active_zset,
--       suspended_zset, terminal_zset, lease_expiry_zset,
--       blocked_deps, blocked_budget, blocked_quota,
--       blocked_route, blocked_operator, suspension_timeout,
--       deps_meta, deps_unresolved,
--       attempt_timeout_zset, execution_deadline_zset,
--       attempts_zset, suspension_current
-- ARGV: execution_id, now_ms, partition_number

local core = redis.call("HMGET", KEYS[1],
  "lifecycle_phase", "ownership_state", "eligibility_state",
  "priority", "created_at", "completed_at",
  "lease_expires_at", "delay_until",
  "current_suspension_id")

local lp, os, es = core[1], core[2], core[3]
local suspension_id = core[9]

-- Suspension state consistency check (defense-in-depth for OOM partial writes)
-- KEYS[19] = ff:exec:{p:N}:<eid>:suspension:current
if lp == "suspended" then
  -- Check: exec says suspended, but does the suspension record exist?
  local susp_exists = redis.call("EXISTS", KEYS[19])
  if susp_exists == 0 then
    -- Zombie: exec_core says suspended but no suspension record.
    -- OOM during suspend_execution before exec_core HSET, then
    -- the reverse happened: exec_core was written but suspension wasn't.
    -- Or suspension was closed but exec_core not updated.
    -- Fix: transition to runnable + eligible_now.
    redis.call("HSET", KEYS[1],
      "lifecycle_phase", "runnable",
      "ownership_state", "unowned",
      "eligibility_state", "eligible_now",
      "blocking_reason", "waiting_for_worker",
      "blocking_detail", "",
      "terminal_outcome", "none",
      "attempt_state", "attempt_interrupted",
      "public_state", "waiting",
      "current_suspension_id", "",
      "current_waitpoint_id", "",
      "last_transition_at", ARGV[2], "last_mutation_at", ARGV[2])
    lp = "runnable"; es = "eligible_now"  -- continue with corrected state
    -- Log warning: reconciler fixed suspended-without-suspension zombie
  end
elseif lp ~= "suspended" and suspension_id ~= nil and suspension_id ~= false and suspension_id ~= "" then
  -- Check: exec NOT suspended, but has a current_suspension_id referencing an open suspension
  local closed_at = redis.call("HGET", KEYS[19], "closed_at")
  if closed_at == nil or closed_at == false or closed_at == "" then
    -- Zombie: suspension record is open but exec_core is not suspended.
    -- OOM during resume/cancel/deliver_signal: suspension was not closed
    -- before exec_core was updated. Close the orphaned suspension.
    redis.call("HSET", KEYS[19],
      "closed_at", ARGV[2],
      "close_reason", "reconciler_cleanup")
    redis.call("HSET", KEYS[1],
      "current_suspension_id", "",
      "current_waitpoint_id", "")
    -- Log warning: reconciler closed orphaned suspension record
  end
end
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
  elseif es == "blocked_by_dependencies" then
    -- Zero-dep promotion: if no deps remain, override to eligible
    local unsatisfied = tonumber(redis.call("HGET", KEYS[14], "unsatisfied_required_count") or "0")
    local unresolved_count = redis.call("SCARD", KEYS[15])
    if unsatisfied == 0 and unresolved_count == 0 then
      -- Override: promote to eligible (deps resolved or never applied)
      redis.call("HSET", KEYS[1],
        "eligibility_state", "eligible_now",
        "blocking_reason", "waiting_for_worker",
        "blocking_detail", "",
        "public_state", "waiting",
        "last_transition_at", tonumber(ARGV[2]) or 0,
        "last_mutation_at", tonumber(ARGV[2]) or 0)
      correct_key = KEYS[2]  -- eligible
      local pri = tonumber(core[4]) or 0
      local created = tonumber(core[5]) or 0
      score = 0 - (pri * 1000000000000) + created
    else
      correct_key = KEYS[8]; score = 0
    end
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

-- Clean orphan entries from auxiliary indexes not in all_sched.
-- These indexes are only valid in specific lifecycle phases:
--   lease_expiry (KEYS[7]):        only when active
--   suspension_timeout (KEYS[13]): only when suspended
--   attempt_timeout (KEYS[16]):    only when active (running attempt)
--   execution_deadline (KEYS[17]): valid when active or runnable (total deadline)
if lp ~= "active" then
  redis.call("ZREM", KEYS[7], eid)   -- lease_expiry orphan
  redis.call("ZREM", KEYS[16], eid)  -- attempt_timeout orphan
end
if lp ~= "suspended" then
  redis.call("ZREM", KEYS[13], eid)  -- suspension_timeout orphan
end
-- execution_deadline: clean only for terminal/suspended (runnable may have valid deadline)
if lp == "terminal" or lp == "suspended" then
  redis.call("ZREM", KEYS[17], eid)  -- execution_deadline orphan
end

-- Orphaned attempt cleanup: KEYS[18] = attempts_zset
local total_ct = tonumber(redis.call("HGET", KEYS[1], "total_attempt_count") or "0")
if total_ct > 0 then
  local latest = redis.call("ZREVRANGE", KEYS[18], 0, 0, "WITHSCORES")
  if #latest >= 2 then
    local latest_idx = tonumber(latest[1])
    if latest_idx and latest_idx >= total_ct then
      -- Orphan: attempt at index >= total_attempt_count. A script created
      -- the attempt but OOM'd before incrementing the counter on exec_core.
      for oi = total_ct, latest_idx do
        redis.call("ZREM", KEYS[18], tostring(oi))
        -- DEL orphaned attempt hash + usage + policy (partition-local)
      end
    end
  end
end

return ok("reconciled")
```

| Property | Value |
|---|---|
| **Per-item action** | Atomic Lua: HMGET state, ZADD correct, ZREM all other scheduling indexes, clean orphan auxiliary indexes, check attempts ZSET for orphans at index >= `total_attempt_count` and ZREM + DEL if found. |
| **Failure mode** | Safe to crash. Desync persists until next scan. |
| **Idempotency** | ZADD on existing member is no-op. ZREM on non-member is no-op. |
| **Batch size** | 100-200 SCAN iterations. Each Lua: ~17 keys, 1 HMGET + 1 ZADD + ~10 ZREM (scheduling) + ~4 ZREM (auxiliary). |
| **Discovery strategy** | Iterate `ff:idx:{p:N}:all_executions` to get execution IDs in the partition. For partitions with ≤10K members: use `SMEMBERS` (single response). For partitions with >10K members: use `SSCAN` with COUNT 200 to avoid large single-response payloads (~7 MB at 195K members). For each execution_id: read lane_id from exec_core, construct per-lane index keys, call `FCALL ff_reconcile_execution_index`. Process in batches of 100-200 per cycle. |

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

### 6.19 Flow Retention Scanner

**Source:** RFC-007 (Flow Container and DAG Semantics), RFC-010 §9.3

**Purpose:** Purges terminal flow records that exceeded their retention window. Cascades to all flow-structural sub-keys on `{fp:N}`. Execution-local member keys are purged separately by the terminal execution retention scanner (§6.12).

| Property | Value |
|---|---|
| **Trigger** | Periodic timer |
| **Frequency** | Every 60-120 seconds per flow partition |
| **Partition scope** | Per `{fp:N}` flow partition |
| **Index** | `ff:idx:{fp:N}:terminal_flows` (ZSET, member: `flow_id`, score: flow terminal transition timestamp ms) |
| **Operation** | `ZRANGEBYSCORE ff:idx:{fp:N}:terminal_flows -inf <now_ms - retention_ms> LIMIT 0 <batch_size>` |
| **Pre-delete safety check** | For each candidate flow: SMEMBERS `ff:flow:{fp:N}:<flow_id>:members`. For each member execution_id: cross-partition read to verify purged (key-not-found on `ff:exec:{p:N}:<exec_id>:core`). If ANY member still exists, **skip this flow** and retry next cycle. Prevents deleting topology while members still reference it. |
| **Per-item cascade (one Lua on `{fp:N}`)** | 1. SMEMBERS members set. 2. Per member: DEL `member:<exec_id>`. 3. Per member: SMEMBERS `out:<exec_id>` → per edge: DEL `edge:<edge_id>`. DEL `out:<exec_id>`. 4. Per member: DEL `in:<exec_id>`. 5. DEL members set. 6. DEL events stream. 7. DEL summary hash. 8. DEL core hash. 9. ZREM from terminal_flows. |
| **Failure mode** | Safe to crash. Flow stays in terminal_flows until step 9 ZREMs it. Re-scan picks up partial deletions. |
| **Idempotency** | DEL on non-existent keys is a no-op. ZREM on non-existent members is a no-op. |
| **Batch size** | 5-10 flows per cycle. A 100-member, 99-edge flow requires ~400 DEL commands. |

**ZADD to `terminal_flows`:** The flow summary projector (§6.7) ZADDs when it derives `public_flow_state` as `completed`, `failed`, or `cancelled`. The `cancel_flow` operation (§3.9) must also ZADD immediately. Score = terminal transition timestamp. Duplicate ZADDs are idempotent (same member, latest score wins).

**Default retention:** 24 hours after flow terminal transition (configurable per-namespace). Matches execution retention default (§9.3).

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

#### Worst-Case Per-Execution Memory (Pathological but Bounded)

| Component | Pathological Scenario | Memory |
|---|---|---|
| Core + sub-keys | Fixed | ~12 KB |
| 100 reclaim attempts (max_reclaim_count=100) | Flapping worker, 50 min at 30s TTL | ~130 KB |
| 10K signals at cap (max_signals_per_execution) | Webhook retry storm | ~4 MB |
| Active stream (MAXLEN 10K × 150B) | Long streaming | ~1.5 MB |
| 50 suspensions (waitpoints + conditions) | Multi-step agent | ~42 KB |
| Lease history (MAXLEN 100) | Capped | ~10 KB |
| **Realistic worst case** | 100 reclaims (no streaming) + 10K signals | **~4.2 MB** (~90× avg) |
| **Extreme worst case** | 100 reclaims each with full streams in retention | **~150 MB** |

Extreme case requires each reclaim to produce 10K stream frames AND all 100 streams within 24h retention. Mitigation: reduce `retention_maxlen` or `retention_ttl_ms` for high-reclaim lanes.

**Growth caps (after round 23 fixes):** Lease history: MAXLEN 100. Streams: MAXLEN 10K/stream. Signals: 10K/execution. Attempts: ~max_reclaim(100)+max_retries+max_replay(10). Waitpoints SET: unbounded but grows only 1 per suspension episode.

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

**Scaling beyond 1M active:** At 10M active executions with default 24-hour retention (10:1 terminal ratio): 110M total × 47 KB ≈ 5.2 TB. A typical 48-shard cluster with 64 GB per shard provides ~3 TB — insufficient. To reach 10M active: reduce terminal retention to 4-6 hours (ratio ~4:1, total ~50M × 47 KB ≈ 2.35 TB), or increase shard count to 96+, or apply aggressive stream MAXLEN (reducing the 47 KB weighted average). Memory is the first scaling bottleneck — claim throughput and Lua latency remain comfortable at this scale.

### 7.4 Hot-Path Memory Considerations

| Hot path | Access pattern | Memory concern |
|---|---|---|
| Claim (scheduler) | ZRANGEBYSCORE on eligible set | Set size proportional to waiting executions per lane per partition. |
| Lease renewal | HSET on execution core + lease current | Two small writes. Negligible. |
| Usage report | HINCRBY on attempt usage + budget usage | Counter increments. Negligible. |
| Stream append | XADD on attempt stream | Frame payload size. Enforce 64KB max (RFC-006). |
| Signal delivery | HSET signal hash + ZADD indexes | Signal payload stored separately. |

No hot path requires loading the full execution record. The separation of core hash, payload, result, policy, and tags (RFC-001 §9.1) ensures hot-path operations touch minimal data.

**Per-execution key count:** A typical execution lifecycle (create → claim → stream → suspend → signal → resume → complete) creates **~22 dedicated Valkey keys** (hashes, strings, streams, sets) plus **~11 entries in shared sorted sets/sets** (eligible, active, terminal, lease_expiry, suspension_timeout, attempt_timeout, all_executions, namespace_index, worker_leases, etc.). Simpler executions (no suspension, no streaming) create ~12 keys. This count drives the cleanup cascade cost (§9.3).

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

### 7.6 End-to-End Latency Estimates

Happy-path latency for key execution lifecycle operations. Assumes: Valkey on same-region network (<1ms RTT), scheduler running co-located, worker already running and idle.

**Submit to first token:**

| Step | Latency | Notes |
|---|---|---|
| `create_execution` (pipeline/MULTI) | ~1ms | Hash creation + ZADD eligible + SET NX idem |
| Scheduler cycle (pick candidate) | ~5-15ms | Poll interval + fairness evaluation |
| `issue_claim_grant` (Lua) | ~0.1ms | On {p:N} |
| `claim_execution` (Lua) | ~0.1ms | On {p:N} |
| Worker receives grant + claims | ~1-2ms | Network RTT |
| Worker starts processing + first `append_frame` | ~0.05ms | On {p:N} |
| **Total** | **~8-20ms** | Dominated by scheduler poll interval |

**Signal to resume (tool call round-trip, excluding external tool time):**

| Step | Latency | Notes |
|---|---|---|
| `deliver_signal` (Lua, signal satisfies condition) | ~0.1ms | On {p:N}, includes resume transition |
| Scheduler cycle (pick resumed candidate) | ~5-15ms | Poll interval |
| `issue_claim_grant` + `claim_resumed_execution` | ~0.2ms | On {p:N} |
| Worker receives + reads continuation | ~1-2ms | Network RTT + get_suspension |
| **Total** | **~7-18ms** | Dominated by scheduler poll interval |

**Full tool-call round-trip (suspend → external tool → signal → resume):**

| Step | Latency | Notes |
|---|---|---|
| `create_pending_waitpoint` + `suspend_execution` | ~0.2ms | On {p:N} |
| External tool execution | variable | Not engine latency |
| Signal delivery + resume + claim | ~7-18ms | See above |
| **Total engine overhead** | **~8-20ms** | Excludes external tool time |

**Key insight:** The scheduler poll interval (5-15ms) dominates. All Valkey Lua operations are sub-millisecond. To reduce latency below 10ms: decrease scheduler poll interval (costs more CPU) or implement push-based scheduling (Valkey keyspace notifications or the lifecycle event stream extension §9.2).

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

### 8.3 Crate Architecture and Process Placement

FlowFabric is implemented as a workspace of Rust crates sharing `ferriskey` for Valkey transport:

| Crate | Purpose | Depends On |
|---|---|---|
| `ferriskey` | Valkey cluster client: FCALL, FUNCTION LOAD REPLACE, pipelines, TLS, cluster routing. | (standalone) |
| `ff-core` | Types, enums, state vector, partition math, key construction, error types. | (standalone) |
| `ff-script` | Typed FCALL wrappers (`ff_complete_execution(...)→CompleteResult`). Macro-generated. Lua library source + loader. | ff-core, ferriskey |
| `ff-engine` | Cross-partition dispatch (§3 operations). Scanner module (§6). Partition router. | ff-script, ff-core, ferriskey |
| `ff-scheduler` | Claim-grant cycle (§3.1), fairness (§5), capability matching (§7), lane management. Does NOT depend on ff-engine — owns its own dispatch for the claim sequence. | ff-script, ff-core, ferriskey |
| `ff-sdk` | Worker SDK (§10). Uses ff-engine for cross-partition orchestrators (report_usage, suspend_and_wait). Uses ff-script directly for single-partition ops (complete, fail, renew, append_frame). Scheduler client trait for grant requests. | ff-engine, ff-script, ff-core, ferriskey |
| `ff-server` | Binary: config, gRPC API, startup, composes ff-engine (scanners) + ff-scheduler. | all crates |
| `ff-test` | Integration test harness: Valkey fixtures, assertion helpers, scenario builders. | ff-core, ff-script, ferriskey |
| `lua/` | Lua source files (helpers.lua, execution.lua, etc.) concatenated by build.rs into flowfabric library. | (build-time only) |

**Process model (v1):**

| Process | Crate Composition | Instances |
|---|---|---|
| **ff-server** | ff-engine (scanners as tokio tasks) + ff-scheduler (claim loop as tokio task) + gRPC API | 2-3 (active-active) |
| **Worker** | ff-sdk (imports ff-engine for orchestration + ff-script for direct FCALL) | N (scaled to workload) |

The scheduler is embedded in ff-server for v1. The `ff-scheduler` crate boundary enables extraction to a separate process in v2 (partition-affine scheduling at high scale). Multiple ff-server instances operate concurrently — the claim-grant model (RFC-009) prevents double-claims atomically.

**No leader election required for v1.** Duplicate scheduling work is wasted but not incorrect.

**Partition configuration:** Partition counts (`num_execution_partitions`, `num_flow_partitions`, `num_budget_partitions`, `num_quota_partitions`) are stored in `ff:config:partitions` on Valkey. All processes (ff-server, workers) read this key on startup and reject if it mismatches local config. This prevents silent partition misconfiguration across restarts or deployments.

### 8.4 Background Scanner Distribution

All scanners are implemented in the `ff-engine::scanner` module. There is no separate scanner process — scanners run as **tokio tasks within ff-server**. Each scanner is a `tokio::spawn`'d loop with a configurable interval.

| Scanner | Tokio Task | Parallelism |
|---|---|---|
| All partition-scoped scanners (lease expiry, delayed promoter, timeout, suspension, etc.) | One task per scanner type, pipelines across all partitions per cycle | Partition batches parallelized via ferriskey pipeline |
| Budget/quota reconcilers | Shared reconciler task | Sequential per budget/quota partition |
| Flow summary projector | Dedicated task | Per flow partition, event-driven + periodic |
| Retention scanners (stream, terminal, lease history) | Shared cleanup task | Low priority, sequential |

**Minimum v1 deployment:** 1 ff-server process with all scanner tasks. For production: 2-3 ff-server instances (scanners run on all instances; idempotency guarantees correctness with overlap).

### 8.5 Client Connection Model

FlowFabric workers and control-plane processes connect to Valkey via the **ferriskey cluster client** (the Rust Valkey client being developed in this repository).

**Connection requirements:**
- Cluster-aware: automatic slot discovery, redirect handling, topology refresh.
- Pipeline support: batch commands for efficiency.
- Valkey Functions support: FCALL invocation. On startup: `FUNCTION LOAD REPLACE` the flowfabric library to all primaries; `FCALL ff_version 0` to verify.
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

### 8.7 Failover and Data Loss

Valkey Cluster uses **asynchronous replication**. The primary acknowledges writes before replication to replicas. During primary failover (~1-5s), the last few milliseconds of writes may be lost.

**Worst-case scenario:** Worker calls `complete_execution` → primary ACKs → primary crashes before replicating → replica promotes → completion writes lost → execution still shows `active` → lease expires → reclaim → **duplicate execution**.

**Mitigations:**

| Strategy | Trade-off | Recommendation |
|---|---|---|
| **Worker re-reads state after complete** | +1 RTT per complete. Catches lost writes. | Recommended for all workloads. |
| **AOF appendfsync=always** | Zero data loss. ~10x write latency. | Critical workloads only. |
| **Idempotent execution design** | Application-level. Same result on re-execution. | Best practice regardless. |
| **Accept rare duplicates** | No overhead. Failover × in-flight probability. | Non-critical workloads. |

**Impact estimate:** At 1000 completes/sec with 5s failover: ~5 executions could duplicate. With re-read verification: 0. Failovers are rare (AWS ElastiCache Multi-AZ: <30s, ~0 planned).

### 8.8 Recommended Metrics

Counters and gauges the engine process exports (Prometheus/OpenTelemetry). Computed by engine, not stored in Valkey.

**Throughput (per namespace+lane):**

| Metric | Type |
|---|---|
| `ff_executions_created_total` | Counter |
| `ff_executions_claimed_total` | Counter |
| `ff_executions_completed_total` | Counter |
| `ff_executions_failed_total` | Counter |
| `ff_executions_cancelled_total` | Counter |
| `ff_executions_expired_total` | Counter (timeout/deadline) |
| `ff_executions_skipped_total` | Counter (DAG dep failure) |
| `ff_executions_suspended_total` | Counter |
| `ff_executions_resumed_total` | Counter |
| `ff_executions_reclaimed_total` | Counter (worker health) |
| `ff_executions_replayed_total` | Counter (operator action) |

**Queue depth (per lane, from §6.13):**

| Metric | Type |
|---|---|
| `ff_queue_waiting` | Gauge |
| `ff_queue_delayed` | Gauge |
| `ff_queue_active` | Gauge |
| `ff_queue_suspended` | Gauge |
| `ff_queue_blocked_budget` | Gauge |
| `ff_queue_blocked_quota` | Gauge |
| `ff_queue_blocked_route` | Gauge |
| `ff_queue_blocked_operator` | Gauge |
| `ff_queue_blocked_dependencies` | Gauge |

**Latency:**

| Metric | Type |
|---|---|
| `ff_claim_latency_ms` | Histogram |
| `ff_complete_latency_ms` | Histogram |
| `ff_signal_delivery_latency_ms` | Histogram |
| `ff_scheduler_cycle_ms` | Histogram |
| `ff_scanner_cycle_ms` | Histogram (per scanner) |
| `ff_scanner_items_processed_total` | Counter (per scanner) |

**Resources:**

| Metric | Type |
|---|---|
| `ff_budget_usage_ratio` | Gauge (per budget) |
| `ff_budget_hard_breach_total` | Counter (per budget+dimension) |
| `ff_budget_soft_breach_total` | Counter (per budget+dimension) |
| `ff_worker_active_count` | Gauge |
| `ff_worker_claim_count` | Gauge (per worker) |

**Safety and dedup:**

| Metric | Type |
|---|---|
| `ff_signal_limit_exceeded_total` | Counter (per namespace) |
| `ff_signal_dedup_total` | Counter (per namespace) |
| `ff_usage_report_dedup_total` | Counter (network retry indicator) |
| `ff_quota_admission_dedup_total` | Counter |
| `ff_suspension_already_satisfied_total` | Counter (signal-before-suspend race) |
| `ff_reconciler_corrections_total` | Counter (per reconciler+correction_type) |

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
- All 14 scanners/reconcilers (§6 lists 19 designed; 14 implemented in v1, including execution_deadline added in adversarial round 4).
- Idempotent, crash-safe, partition-parallel.

### 9.2 Designed-For but Deferred

**SQLite local mode:** The partition model and key schema are designed to be implementable on a single-node SQLite backend for local development and testing. Deferred because Valkey is the primary backend and the abstraction layer is not yet built.

**Multi-backend abstraction:** The operation semantics (atomicity classes A/B/C) are defined abstractly enough to support alternative backends (PostgreSQL, DynamoDB). Deferred because v1 is Valkey-native and the interface boundary is not yet hardened.

**Global secondary indexes:** Cross-partition search by tag, tenant, time range, or arbitrary field. V1 uses partition-local indexes and namespace-level sorted sets. Full-text or faceted search requires an external index (Elasticsearch, Meilisearch).

**Pub/sub for real-time notifications:** Valkey SUBSCRIBE channels for execution completion notifications, stream frame push, and flow state changes. V1 uses polling (XREAD BLOCK for streams, periodic reads for state). Pub/sub can reduce latency for request/reply and live dashboards.

**AOF/RDB persistence tuning:** V1 uses Valkey defaults or cloud provider defaults (e.g., ElastiCache automatic backups). Production persistence tuning (fsync policy, snapshot frequency, memory fragmentation management) is operational, not architectural.

**Public API wire format (gRPC/REST/SDK):** V1 operations are invoked via Valkey Lua scripts through the ferriskey cluster client. No public API wire format is specified in these RFCs. An API server translating external gRPC/REST requests to Lua script invocations is a v1 companion component — its design is outside the scope of the execution engine RFCs. UC-62 (language-neutral control API) is addressed by the operation semantics defined here; the transport layer is deferred.

**Cross-execution event feed:** V1 does not provide a lane-level or system-level event stream for observers and dashboards. Per-execution observability is available: attempt streams (XREAD BLOCK for live output, RFC-006), lease history streams (lifecycle events, RFC-003). Lane-level dashboards rely on periodic count aggregation (§6.13). A cross-execution event stream (one XADD per state transition, consumable by dashboards and webhooks) is a designed-for extension that would address UC-55 (execution event feed) more fully.

**V1 event model — fragmented sources:** Until the unified event stream is implemented, execution lifecycle events are distributed across 4 sources. Operators building dashboards or audit tools must understand this fragmentation:

| Source | Key Pattern | Contains | Access Pattern |
|---|---|---|---|
| **Lease history** | `ff:exec:{p:N}:<eid>:lease:history` | Ownership transitions: acquired, released, expired, revoked, reclaimed. Each with lease_id, epoch, attempt_index, worker_id, reason. | XREAD BLOCK for tailing, XRANGE for replay. |
| **Waitpoint signal stream** | `ff:wp:{p:N}:<wp_id>:signals` | Signal delivery: signal_id, name, category, source, matched flag. | XRANGE. Requires knowing the waitpoint_id (from suspension record). |
| **Suspension record** | `ff:exec:{p:N}:<eid>:suspension:current` | Current suspension state: reason_code, waitpoint_key, timeout_at, satisfied_at, closed_at, close_reason. | HGETALL. Only shows current/last episode. |
| **Execution core** | `ff:exec:{p:N}:<eid>:core` | Authoritative state vector (all 7 dimensions), timestamps, accounting. | HMGET for specific fields. |

**Simplified dashboard approach:** For operators who want a single-source view without tailing 4 streams, poll `exec_core` for `public_state` at 1-5 second intervals. This gives: waiting → active → suspended → active → completed. The polling misses intermediate events (which signal triggered resume, which worker claimed) but provides a usable timeline. For detailed audit, read all 4 sources.

**Gap between lease_released(suspend) and lease_acquired(resume):** Lease history shows "released/suspend" then a gap until "acquired/resume_after_suspension". During this gap: signal delivery, resume condition evaluation, and the suspended→runnable transition are NOT visible in lease_history. They are visible in the waitpoint signal stream and the execution core state.

**Operator debugging — dependency blocks:** Read `deps:meta` for unsatisfied count. For each edge in `deps:unresolved`: read `dep:<edge_id>` hash for `upstream_execution_id`, then cross-partition read upstream `public_state`. Implementation may wrap this in a convenience `explain_dependency_block(eid)` operation.

**Operator emergency — force-unblock:** No direct `blocked→eligible` bypass exists by design — blocks have causes that should be resolved (`override_budget`, `enable_lane`, `register_worker`). Emergency escape: `cancel_execution` (terminal) followed by `replay_execution` (re-enters scheduling from scratch).

**Per-execution lifecycle event stream (RECOMMENDED v1 extension, not blocking):**

Recommended key: `ff:exec:{p:N}:<execution_id>:events` (Valkey Stream). One XADD per lifecycle transition:

| Event | Trigger | Fields |
|---|---|---|
| `created` | `create_execution` | execution_id, lane_id, kind, creator |
| `claimed` | `claim_execution` / `claim_resumed` | worker_id, attempt_index, lease_epoch |
| `suspended` | `suspend_execution` | suspension_id, waitpoint_key, reason_code |
| `resumed` | `deliver_signal` / `resume_execution` | trigger, resume_delay_ms |
| `completed` | `complete_execution` | attempt_index |
| `failed` | `fail_execution` (terminal) | failure_reason, failure_category |
| `cancelled` | `cancel_execution` | reason, source |
| `reclaimed` | `reclaim_execution` | old_worker, new_worker, new_epoch |
| `replayed` | `replay_execution` | replay_reason, requested_by, new_attempt_index |

Consolidates audit trail (currently across lease history + attempt records + signal records + suspension records) into one queryable timeline. Simplifies `enqueue_and_wait` (subscribe here instead of lease_history). Enables real-time dashboards (XREAD BLOCK). Addresses UC-55. Implementation: one XADD (~100 bytes) per transition, trimmed by MAXLEN, shares `{p:N}`. If deferred: four-source reconstruction works; operators use `get_execution_state` + `list_attempts`.

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
| Flow records | 24 hours after flow completion | Per-flow or per-namespace policy. Enforced by flow retention scanner (§6.19). |
| Budget records | Indefinite (operator-managed) | Operator deletes explicitly |
| Quota policy records | Indefinite (operator-managed) | Operator deletes explicitly |
| Worker registrations | Heartbeat TTL (e.g., 60s) | Per-worker |
| Claim grants | 5 seconds TTL | Global default |
| Idempotency keys | Dedup window TTL (e.g., 24h) | Per-lane |
| Waitpoint key resolution | Waitpoint TTL + grace | Per-waitpoint |

**Cleanup cascade:** When a terminal execution is purged by the retention scanner, all sub-objects are deleted in one batch:
1. **Read before delete:** HGETALL `ff:exec:...:tags` (need tag key-value pairs for step 10). HMGET exec_core (`flow_id`, `lane_id`, `namespace`, `current_worker_id`). Then DEL: execution core hash, payload, result, policy, tags.
2. All attempt hashes, usage hashes, policy hashes (discovered via `ff:exec:...:attempts` ZSET).
3. Attempt sorted set.
4. All stream data and metadata (per attempt, discovered via attempts ZSET).
5. Lease current (if still present) and lease history.
6. DEL `ff:exec:{p:N}:<eid>:suspension:current` (suspension current hash). DEL `ff:exec:{p:N}:<eid>:suspensions` (suspension history stream, if exists).
7. Per-execution signal index and individual signal records **including payload keys** (`ff:signal:{p:N}:<signal_id>` + `ff:signal:{p:N}:<signal_id>:payload`, discovered via `ff:exec:...:signals` ZSET).
8. Dependency records: `ff:exec:{p:N}:<eid>:deps:meta` hash, all `ff:exec:{p:N}:<eid>:dep:<edge_id>` hashes (discovered via SCAN `ff:exec:{p:N}:<exec_id>:dep:*`), `ff:exec:{p:N}:<eid>:deps:unresolved` set.
9. All waitpoint records: for each waitpoint_id in `ff:exec:...:waitpoints` SET (mandatory), DEL the waitpoint hash (`ff:wp:{p:N}:<wp_id>`), signals stream (`ff:wp:{p:N}:<wp_id>:signals`), and condition hash (`ff:wp:{p:N}:<wp_id>:condition`). Then DEL the waitpoints set itself.
10. ZREM from terminal sorted set. ZREM from `ff:ns:<namespace>:executions`. For each tag `(key, value)` read in step 1: SREM from `ff:tag:<namespace>:<key>:<value>`. (Tag and namespace indexes are cross-partition secondary indexes — SREMs are best-effort post-script calls, not part of the atomic Lua.)
11. SREM from `ff:idx:{p:N}:all_executions`.
12. Defensive ZREM/SREM from all non-terminal scheduling and timeout indexes (execution should NOT be in these, but catches any state/index desync): `ff:idx:{p:N}:lane:<lane>:eligible`, `:delayed`, `:active`, `:suspended`, `ff:idx:{p:N}:lane:<lane>:blocked:dependencies`, `:blocked:budget`, `:blocked:quota`, `:blocked:route`, `:blocked:operator`, `ff:idx:{p:N}:lease_expiry`, `ff:idx:{p:N}:suspension_timeout`, `ff:idx:{p:N}:attempt_timeout`, `ff:idx:{p:N}:execution_deadline`, `ff:idx:{p:N}:worker:<worker_id>:leases`. ZREM/SREM on non-existent members are no-ops.

The cascade runs as **one Lua script per execution** (not one script for the entire batch). Each execution cleanup issues ~47 Valkey commands (4 reads + ~23 DELs + ~20 ZREM/SREM) and takes ~0.5ms, blocking the shard briefly. The retention scanner iterates the terminal sorted set and calls the cascade Lua once per execution, processing `batch_size` executions per cycle. All keys share `{p:N}`, ensuring each call is partition-local. The script reads the execution core first (to extract flow_id, lane_id, namespace, tags, worker_id), then the attempts ZSET (for attempt/stream keys), the signals ZSET (for signal keys), and the waitpoints SET (for waitpoint keys), before deleting everything. The defensive ZREM step is cheap (O(log N) per set, ~14 sets) and guarantees no orphaned index entries survive.

Cross-partition references (flow membership, budget attachment) are cleaned lazily: the flow summary projector (§6.7) SREM's stale members on member-not-found, and budget reconcilers remove stale execution references.

### 9.4 Explicit Non-Goals

The following are **not** goals for the v1 Valkey backend:

- **Durable event sourcing:** FlowFabric stores current state + audit history, not a complete event log that can reconstruct state from scratch. Lease history and flow events provide audit trails, but the execution core hash is the source of truth, not a derived projection.
- **Cross-shard transactions:** Valkey Cluster does not support multi-slot transactions. All atomic operations are partition-local. Cross-partition consistency is achieved through idempotent reconciliation, not distributed transactions.
- **Zero-downtime schema migration:** V1 key schemas are designed for forward compatibility (new fields can be added to hashes without migration), but structural changes (e.g., changing partition count) require a planned migration.
- **Built-in observability backend:** FlowFabric exposes metrics and events for external observability (Prometheus, Grafana, OpenTelemetry), but does not include a built-in dashboard or time-series store.
- **Namespace-based access control:** V1 does not enforce namespace/tenant isolation at the engine level. The `namespace` field on execution, flow, and budget objects is metadata used for fairness (RFC-009 §5.3), listing (namespace index), and idempotency scoping — **not a security boundary**. Any caller who knows an `execution_id` can read, signal, or inspect that execution regardless of namespace. Multi-tenant isolation must be enforced at the API gateway or control plane layer. Engine-level namespace enforcement is a designed-for extension.

  **API gateway namespace validation requirements:** The following table lists every operation that involves multiple entities and the namespace check the API gateway MUST perform before invoking the engine. Lua scripts assume namespace correctness for performance — they do NOT validate namespace.

  | Operation | Entities Involved | Required Namespace Check |
  |-----------|-------------------|--------------------------|
  | `create_execution` | caller + namespace | Caller authorized for target namespace. |
  | `attach_budget` | budget + scope entity | Budget's scope includes the target entity's namespace. For scope_type=tenant: scope_id must equal target namespace. For scope_type=execution: verify execution.namespace matches budget owner namespace. |
  | `add_execution_to_flow` | execution + flow | `execution.namespace == flow.namespace`. Cross-namespace flow membership is forbidden. |
  | `deliver_signal` / `send_signal` | caller + target execution | Caller authorized for `execution.namespace`. Waitpoint key MAC prevents guessing, but the gateway must still verify namespace authorization after decoding. |
  | `report_usage` | execution + attached budgets | Verified at `attach_budget` time — no re-check needed per-report. |
  | `claim_execution` | worker + execution | Worker authorized for `execution.namespace` (typically via lane authorization). |
  | `create_child_execution` | parent + child | Child inherits `parent.namespace`. Gateway must reject attempts to set a different namespace on the child. |
  | `cancel_execution` | caller + execution | Caller authorized for `execution.namespace`. |
  | `replay_execution` | caller + execution | Caller authorized for `execution.namespace`. |
  | `suspend_execution` | worker + execution | Validated implicitly by lease ownership (worker already authorized at claim time). |
  | `attach_quota_policy` | quota policy + scope entity | Quota scope includes target namespace. |
  | `cancel_flow` | caller + flow | Caller authorized for `flow.namespace`. |

  **Cross-namespace threat scenarios** (what happens if the gateway fails to validate):
  - **Budget hijack:** Attacker attaches Budget-X (namespace-A, $50K limit) to executions in namespace-B. Namespace-B's usage charges against namespace-A's budget, draining their allowance.
  - **Flow contamination:** Attacker adds namespace-B execution to namespace-A flow. The flow summary counts the foreign execution, skewing aggregate metrics. Dependency resolution could block or skip namespace-A executions based on namespace-B outcomes.
  - **Signal injection:** Attacker crafts a signal targeting namespace-B's waitpoint with a malicious payload. The resumed execution processes the attacker's data as if it were a legitimate callback.
  - **Claim theft:** Attacker's worker claims executions from namespace-B, gaining access to input payloads and the ability to produce results.
- **Trusted network assumption:** The Valkey backend assumes a trusted network between workers, schedulers, reconcilers, and Valkey. All callers are trusted to provide correct parameters (worker_id, lease_id, lease_epoch, usage deltas). The Lua scripts validate data consistency (lease fencing, state vector constraints) but do NOT authenticate callers. Untrusted callers must be mediated by an authenticated API gateway that validates identity, authorization, and input before invoking Lua scripts. Engine-level per-operation authentication (signed tokens, per-worker ACLs) is a post-v1 extension.
- **Separate dead-letter queue:** FlowFabric does not have a DLQ. Terminal failed executions are stored in the per-lane terminal sorted set (`ff:idx:{p:N}:lane:<lane_id>:terminal`) alongside completed/cancelled/expired/skipped executions. Operators find failed executions via `list_executions(lane_id, public_state=failed)`. The `failure_category` field on the attempt record (RFC-002) distinguishes failure causes (`timeout`, `worker_error`, `provider_error`, `budget_exceeded`). Failed executions are replayable via `replay_execution` (RFC-001 §4.4). This design matches UC-07 (dead-letter handling): "move terminal failures into a durable failed state for inspection or replay."

---

## 10. Worker SDK Contract (`ff-sdk`)

The `ff-sdk` crate provides the worker-facing API. Workers `use ff_sdk::FlowFabricWorker`. The SDK depends on `ff-engine` for cross-partition orchestrators (`report_usage`, `suspend_and_wait`) and on `ff-script` directly for single-partition operations (`complete`, `fail`, `renew`, `append_frame`). Grant requests go through a `SchedulerClient` trait (gRPC or in-process).

### 10.1 Communication Model

The scheduler is a separate stateless process (§8.3). Workers request work via the scheduler's claim API. The scheduler performs fairness, capability matching, quota, and budget checks, then issues a `claim_grant` on the execution's `{p:N}` partition. The worker then calls `acquire_lease` (`claim_execution` or `claim_resumed_execution`) directly on Valkey. Two hops per claim: worker → scheduler (request), worker → Valkey (acquire).

**Transport between worker and scheduler is implementation-defined.** The RFCs specify the Valkey Lua script interface; the scheduling protocol is left to the SDK/deployment. Recommended approaches:
- **(a) gRPC** (recommended for low-latency claim cycles): scheduler exposes a `ClaimService.RequestClaim(caps, lanes)` RPC. Workers discover scheduler instances via service mesh or DNS.
- **(b) HTTP/2**: scheduler exposes REST/JSON endpoint. Simpler but higher per-request overhead.
- **(c) Valkey Streams**: worker XADDs to `ff:sched:requests:<scheduler_id>`, scheduler XREADs, writes grant, responds via a per-worker reply stream. Zero additional infrastructure but adds latency.

The choice affects claim latency (gRPC: ~1ms, HTTP: ~2-5ms, Streams: ~5-15ms) but not correctness — the scheduler's output is always a `claim_grant` hash on `{p:N}`.

### 10.2 Recommended Timing Defaults

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Lease TTL | 30s | Long enough for slow operations; short enough for prompt reclaim. |
| Lease renewal interval | 10s (TTL / 3) | Renew at 1/3 of TTL gives 2 missed renewals before expiry. |
| Heartbeat interval | 15s | Keeps worker registration alive. TTL = 60s (4× heartbeat). |
| Claim backoff on empty | 100ms–1s | Exponential backoff when no eligible execution found. |

### 10.3 Minimum Viable Worker Loop

```
STARTUP:
  register_worker(worker_id, instance_id, capabilities)     [RFC-009]
  start heartbeat_worker(instance_id) every 15s             [RFC-009]

CLAIM:
  1. claim_request(caps, lanes) → scheduler issues grant    [RFC-009]
     Scheduler returns to SDK: {execution_id, partition_id,
       lane_id, attempt_state, lease_ttl_ms, attempt_timeout_ms,
       execution_deadline_at}
     SDK uses partition_id for {p:N} key construction,
     attempt_state for claim dispatch (§4.6).
  2. IF attempt_state == attempt_interrupted:
       claim_resumed_execution → same attempt, new lease    [RFC-001]
       read continuation: get_suspension(exec_id) to get
         continuation_metadata_pointer from closed record
     ELSE:
       claim_execution → new attempt + lease                [RFC-001]

  2b. Read execution context (after successful claim):
      GET ff:exec:{p:N}:<exec_id>:payload     → input data
      GET ff:exec:{p:N}:<exec_id>:policy      → ExecutionPolicySnapshot
      HMGET ff:exec:{p:N}:<exec_id>:core      → execution_kind,
            payload_encoding, namespace, lane_id (for routing context)

PROCESS (loop while working):
  3. renew_lease(exec_id, lease_id, epoch) every 10s        [RFC-003]
  4. append_frame(exec_id, att_idx, lease_id, epoch, frame) [RFC-006]
  5. report_usage(exec_id, att_idx, epoch, delta, seq)      [RFC-008]
     SDK internally: (a) HINCRBY on {p:N} via report_usage_on_attempt
                     (b) check-before-increment on {b:M} per budget
     Returns: {ok, breach: null} or {ok, breach: {dim, current, limit}}
     On HARD_BREACH: SDK auto-calls fail_execution(budget_exceeded)
     Worker MUST stop processing after breach (see §10.4)

DONE:
  6a. complete_execution(exec_id, lease_id, epoch, att_id)  [RFC-001]
  6b. fail_execution(exec_id, lease_id, epoch, att_id, ...) [RFC-001]
      NOTE: complete/fail are single-partition for the primary FCALL.
      Post-FCALL obligations (quota DECR on {q:K}, dependency
      resolution dispatch on {fp:N}→{p:N}) are cross-partition,
      handled by ff-engine::dispatch within the same ff-sdk call.
      The worker sees a single async call; the SDK orchestrates
      the multi-partition follow-up transparently.

SUSPEND (external waits):
  7a. create_pending_waitpoint → returns waitpoint_key      [RFC-004]
  7b. call external system with waitpoint_key
  7c. suspend_execution                                     [RFC-004]
      IF returns ALREADY_SATISFIED:
        do NOT exit loop — lease still held
        read matched signal payloads, continue processing
        (external result arrived before suspension committed)
      ELSE (returns OK):
        lease released, exit processing loop
        on resume: re-claim via step 2 (claim_resumed)
        worker reads continuation_metadata_pointer to resume
  7d. IF worker decides NOT to suspend after step 7a
      (e.g., external call returned synchronously, or worker
       chooses move_to_waiting_children instead):
        call close_waitpoint(waitpoint_ref, reason)         [RFC-004]
        prevents orphaned pending waitpoints (otherwise cleaned
        by pending_wp_expiry scanner in 30-60s)

SHUTDOWN (crash):
  deregister_worker(instance_id) — leases expire naturally  [RFC-009]
  Lease expiry scanner detects + reclaim handles recovery.

GRACEFUL SHUTDOWN:
  1. Stop requesting new claims from scheduler.
  2. For each active execution:
     (a) If work can complete quickly (< remaining lease TTL):
         complete_execution or fail_execution normally.
     (b) If work is long-running (e.g., LLM call in progress):
         call delay_execution(exec_id, lease_id, epoch, now_ms + 1)
         to release lease immediately. Execution returns to
         scheduling and is re-claimed by another worker.
     (c) If neither is possible: let lease expire. Execution is
         reclaimed within lease_ttl (default 30s).
  3. deregister_worker(instance_id)                          [RFC-009]
  4. Close Valkey connections.
```

**Continuation on resume:** When `claim_resumed_execution` returns, the worker must read the closed suspension record via `get_suspension(exec_id)` (RFC-004) to retrieve the `continuation_metadata_pointer`. This pointer references worker-owned durable state (e.g., last completed step, checkpoint data, tool call context) that the worker needs to resume processing from where it left off. The engine does not own the continuation state — it stores only the pointer.

### 10.4 Error Handling

| Error | Action |
|-------|--------|
| `no_eligible_execution` | Backoff 100ms-1s, retry. |
| `claim_grant_expired` | Retry from step 1. |
| `stale_lease` / `lease_expired` / `lease_revoked` | Stop. Do NOT complete/fail. Reclaimed. |
| `execution_not_active` | On retry: check enriched return. Epoch match + success = my call won. |
| `budget_exceeded` | **Immediate stop.** Worker MUST cease resource consumption (stop LLM calls, stop appending frames). Call `fail_execution` with `failure_category = "budget_exceeded"`. Do NOT call `complete_execution`. Enforcement is cooperative: both worker and engine may call `fail_execution` — the second caller receives `execution_not_active` or `execution_already_terminal`, which is expected and harmless. If `fail_execution` returns `execution_not_active` with `terminal_outcome = failed` and matching `lease_epoch`: the engine applied enforcement first. Treat as successful enforcement and stop work. |
| `budget_soft_exceeded` | Log warning. Worker MAY continue — soft limits are advisory in v1. |
| `signal_limit_exceeded` | Signal cap reached (default 10K per execution). Stop sending signals. Use idempotency keys to reduce unique signal count. |
| `max_reclaims_exceeded` | Execution terminated by engine due to excessive reclaims (flapping worker). Investigate worker stability. |

### 10.5 SDK Requirements

- Cluster-aware Valkey client: slot discovery, redirects, FCALL support.
- Independent heartbeat thread: must run even if processing blocks.
- Independent lease renewal thread: must fire during slow LLM calls.
- Partition awareness: track `partition_id` per execution for `{p:N}` keys.
- Parse structured returns per §4.9: `{1, "OK", ...}` or `{0, "ERROR", ...}`.
- **Monotonic `usage_report_seq`:** SDK maintains a per-attempt counter (starts at 1, increments on each `report_usage` call). Passed as ARGV to `report_usage_on_attempt`. Enables idempotent retries — if the call succeeds but the ACK is lost, the retry with the same seq is a no-op.

### 10.6 Idempotency

**Submission idempotency:** The SDK SHOULD auto-generate an `idempotency_key` for every `create_execution` call if the caller does not provide one. Recommended format: `<client_id>:<monotonic_counter>` or UUID. Without an idempotency key, a retry after a lost ACK creates a **duplicate execution** — two distinct executions with different `execution_id`s processing the same input. The engine's `SET NX` dedup guard (RFC-001 §4.1) only fires if an `idempotency_key` is provided.

**Usage reporting idempotency:** Each `report_usage` call includes a monotonic `usage_report_seq` (see §10.5). The Lua script checks `attempt_usage.last_usage_report_seq` and skips duplicate increments. This prevents double-counting tokens and cost on network retries.

**Signal idempotency:** External systems sending signals SHOULD include an `idempotency_key` (RFC-005 §7.2). The engine's `SET NX` guard deduplicates within the `signal_dedup_window` (default 24h).

**Claim idempotency:** `claim_execution` consumes (DELs) the claim grant key atomically. A retry finds the grant missing → `invalid_claim_grant`. The worker must request a new grant from the scheduler. This is safe — the first call either succeeded (worker has a lease) or the grant expired (reconciler re-adds to eligible).

### 10.7 Error Classification

Every error code returned by worker-facing scripts, classified by SDK action.

**TERMINAL** — Stop work. Do NOT retry. **RETRYABLE** — Retry after backoff. **COOPERATIVE** — Take specific action then stop. **INFORMATIONAL** — No-op, log and continue.

| Error Code | Class | SDK Action |
|---|---|---|
| `stale_lease` | TERMINAL | Stop. Lease superseded by reclaim. Do NOT complete/fail. |
| `lease_expired` | TERMINAL | Stop. Lease TTL elapsed. Reclaim scanner handles. |
| `lease_revoked` | TERMINAL | Stop. Operator revoked. |
| `execution_not_active` | TERMINAL | Stop. Check enriched return: epoch match + `success` = your completion won. |
| `active_attempt_exists` | TERMINAL | Bug. Log error. |
| `use_claim_resumed_execution` | RETRYABLE | Re-dispatch to `claim_resumed_execution`. |
| `not_a_resumed_execution` | RETRYABLE | Re-dispatch to `claim_execution`. |
| `execution_not_leaseable` | RETRYABLE | State changed since grant. Request new grant. |
| `lease_conflict` | RETRYABLE | Another worker holds lease. Request different execution. |
| `invalid_claim_grant` | RETRYABLE | Grant missing/mismatched. Request new grant. |
| `claim_grant_expired` | RETRYABLE | Grant TTL elapsed. Request new grant. |
| `no_eligible_execution` | RETRYABLE | Backoff 100ms-1s, retry. |
| `budget_exceeded` | COOPERATIVE | **Immediate stop.** Call `fail_execution(failure_category="budget_exceeded")`. |
| `budget_soft_exceeded` | INFORMATIONAL | Log warning. Continue. |
| `execution_not_suspended` | INFORMATIONAL | Already resumed/cancelled. No-op. |
| `already_suspended` | INFORMATIONAL | Open suspension exists. No-op. |
| `waitpoint_closed` | INFORMATIONAL | Signal too late. Return to caller. |
| `waitpoint_not_found` | RETRYABLE | Waitpoint may not exist yet. Retry with backoff. |
| `target_not_signalable` | TERMINAL | Execution not suspended, no pending waitpoint. |
| `waitpoint_pending_use_buffer_script` | RETRYABLE | Route to `buffer_signal_for_pending_waitpoint`. |
| `duplicate_signal` | INFORMATIONAL | Dedup. Return existing signal_id. |
| `payload_too_large` | TERMINAL | Payload > 64KB. Reduce or use reference. |
| `signal_limit_exceeded` | TERMINAL | Max signals reached. Likely webhook storm. |
| `invalid_waitpoint_key` | TERMINAL | MAC failed. Token invalid or expired. |
| `execution_not_terminal` | TERMINAL | Cannot replay non-terminal. |
| `max_replays_exhausted` | TERMINAL | Replay limit reached. Operator increases. |
| `stream_closed` | TERMINAL | Attempt terminal. No appends. |
| `stale_owner_cannot_append` | TERMINAL | Lease mismatch on stream append. |
| `retention_limit_exceeded` | TERMINAL | Frame > 64KB. Reduce size. |
| `invalid_lease_for_suspend` | TERMINAL | Lease/attempt binding mismatch. |
| `resume_condition_not_met` | TERMINAL | Conditions not satisfied, no override. |
| `execution_not_eligible` | INFORMATIONAL | State changed. Scheduler skips. |
| `execution_not_in_eligible_set` | INFORMATIONAL | Another scheduler got it. Skip. |
| `grant_already_exists` | INFORMATIONAL | Grant already issued. Skip. |
| `execution_not_reclaimable` | INFORMATIONAL | Already reclaimed/cancelled. Skip. |
| `invalid_dependency` | TERMINAL | Edge doesn't exist. Data issue. |
| `stale_graph_revision` | RETRYABLE | Re-read adjacency, retry. |
| `execution_already_in_flow` | TERMINAL | Already in another flow. |
| `cycle_detected` | TERMINAL | Edge would create cycle. Reject. |
| `flow_not_found` | TERMINAL | Flow does not exist. |
| `flow_already_terminal` | TERMINAL | Flow is cancelled/completed/failed. Mutation rejected. |
| `execution_not_in_flow` | TERMINAL | Execution not a member of the specified flow. |
| `dependency_already_exists` | TERMINAL | Edge already exists between these executions. |
| `self_referencing_edge` | TERMINAL | Upstream and downstream are the same execution. |
| `deps_not_satisfied` | INFORMATIONAL | Dependencies still unresolved (for promote_blocked). |
| `not_blocked_by_deps` | INFORMATIONAL | Execution not blocked by dependencies. |
| `ok_already_applied` | INFORMATIONAL | Usage seq already processed. No-op. |
| `no_active_lease` | INFORMATIONAL | Revoke target has no active lease. No-op. |
| `not_runnable` | TERMINAL | Execution not in runnable state (for block/unblock). |
| `invalid_blocking_reason` | TERMINAL | Unrecognized blocking reason string. |
| **RFC-004 Suspension/Waitpoint** | | |
| `pending_waitpoint_expired` | INFORMATIONAL | Pending waitpoint aged out before suspension committed. |
| `waitpoint_not_pending` | TERMINAL | Waitpoint not in pending state. |
| `waitpoint_already_exists` | INFORMATIONAL | Waitpoint already exists (pending or active). |
| `waitpoint_not_open` | INFORMATIONAL | Waitpoint not in pending or active state. |
| `invalid_waitpoint_for_execution` | TERMINAL | Waitpoint/execution binding mismatch. |
| **RFC-002 Attempt** | | |
| `attempt_not_found` | TERMINAL | Attempt index doesn't exist. |
| `active_attempt_exists` | BUG | Should never reach — invariant violation. Log + alert. |
| `attempt_not_in_created_state` | BUG | Attempt not created. Internal sequencing error. |
| `attempt_not_started` | TERMINAL | Attempt not running. Cannot end/report. |
| `attempt_already_terminal` | INFORMATIONAL | Already ended. No-op. |
| `execution_not_found` | TERMINAL | Execution doesn't exist. |
| `execution_not_eligible_for_attempt` | TERMINAL | Wrong state for new attempt. |
| `replay_not_allowed` | TERMINAL | Not terminal or limit reached. |
| `max_retries_exhausted` | TERMINAL | Retry limit reached. |
| **RFC-006 Stream** | | |
| `stream_not_found` | EXPECTED | No frames appended yet. Normal for new attempts. |
| `stream_already_closed` | INFORMATIONAL | Already closed. No-op. |
| `invalid_frame_type` | SOFT_ERROR | Unrecognized type. Log warning, may accept if open enum. |
| `invalid_offset` | RETRYABLE | Invalid Stream ID. Fix offset and retry. |
| `unauthorized` | TERMINAL | System/operator auth failed. |
| **RFC-008 Budget/Quota** | | |
| `budget_not_found` | TERMINAL | Budget doesn't exist. |
| `invalid_budget_scope` | TERMINAL | Malformed scope. |
| `budget_attach_conflict` | TERMINAL | Budget already attached or conflicts. |
| `budget_override_not_allowed` | TERMINAL | No operator privileges. |
| `quota_policy_not_found` | TERMINAL | Quota doesn't exist. |
| `rate_limit_exceeded` | RETRYABLE | Window full. Backoff `retry_after_ms`. |
| `concurrency_limit_exceeded` | RETRYABLE | Cap hit. Backoff and retry. |
| `quota_attach_conflict` | TERMINAL | Policy already attached. |
| `invalid_quota_spec` | TERMINAL | Malformed policy definition. |

**Classification key:** TERMINAL (give up), RETRYABLE (backoff+retry), COOPERATIVE (worker must act), INFORMATIONAL (no-op, log), ADVISORY (log warning only), BUG (should never happen — alert), EXPECTED (normal empty state), SOFT_ERROR (log warning, may continue).

---

## 11. Recommended Implementation Order

Build incrementally. Each phase adds a testable capability. All functions are registered in the `flowfabric` library via `FUNCTION LOAD REPLACE`.

| Phase | Capability | Functions | Scanners | Crates | RFCs |
|---|---|---|---|---|---|
| **0. Library bootstrap** | `FUNCTION LOAD REPLACE` with shared helpers + `ff_version` | `ff_version` + shared helpers | — | ferriskey (FCALL), ff-core, ff-script (loader + lua/) | RFC-010 |
| **1. Hello world** | create → claim → renew → complete, cancel, delay, priority, progress | 12: create, claim, complete, renew, mark_expired, cancel, issue_claim_grant, revoke_lease, delay_execution, move_to_waiting_children, change_priority, update_progress | Lease expiry, delayed promoter, index reconciler | + ff-engine (minimal dispatch + 3 scanners), ff-scheduler (minimal single-lane), ff-sdk (register/claim/complete/delay), ff-server, ff-test | RFC-001, RFC-002, RFC-003, RFC-009 |
| **2. Failure + retry + reclaim** | fail → retry → backoff → reclaim → expire | 5: fail, issue_reclaim_grant, reclaim, expire_execution, promote_delayed | Attempt timeout scanner | + ff-engine (reclaim discovery), ff-script (+5 wrappers) | RFC-001, RFC-002, RFC-003 |
| **3. Suspend + signal + resume** | suspend → signal → resume → re-claim, timeout | 8: suspend, deliver_signal, claim_resumed, create_pending_waitpoint, buffer_signal, expire_suspension, resume_execution, close_waitpoint | Suspension timeout, pending wp expiry | + ff-engine (suspend_and_wait orchestrator), ff-sdk (+suspend/resume) | RFC-004, RFC-005 |
| **4. Streaming** | append frames, tail, close on terminal | 1: append_frame | Stream retention trimmer | + ff-sdk (+stream append/tail) | RFC-006 |
| **5. Budget + quota** | usage reporting, breach, admission | 5: report_usage_on_attempt, report_usage_and_check, check_admission_and_record, block_execution, unblock_execution | Budget reset, budget reconciler, quota reconciler, unblock scanner | + ff-engine (report_usage orchestrator), ff-scheduler (+budget/quota checks) | RFC-008, RFC-009 |
| **6. Flow coordination** | deps, resolution, skip, replay, eligibility | 6: stage_dependency_edge, apply_dependency_to_child, resolve_dependency, replay_execution, evaluate_flow_eligibility, promote_blocked_to_eligible | Dependency reconciler, flow summary projector | + ff-engine (dep resolution dispatch, flow cancellation) | RFC-007 |
| **Total** | All core paths | **43 functions** (31 unique after overlap dedup) | 14 scanners | |

**Phase 1 is the minimum viable execution engine.** It proves the partition model, claim-grant mechanism, lease fencing, state vector management, and basic operator control (cancel). `renew_lease` is essential even for "Hello World" — without it, every execution expires after 30s. `cancel_execution` is needed for operator control of stuck executions. The delayed promoter is included because `create_delayed_execution` is a Phase 1 submission mode.

**Phase 2 notes:** `fail_execution` sets `pending_retry_attempt` + lineage fields on exec core. It does NOT create the attempt — `claim_execution` (Phase 1) reads these fields on the next claim and creates the attempt with the correct type. This works because Phase 1's `claim_execution` uses `or ""` defaults for the pending fields (§4.8m forward compatibility). `reclaim_execution` is essential for crash recovery. `expire_execution` handles both per-attempt timeouts (slow-but-alive workers) and total execution deadlines, across all lifecycle phases (active, runnable, suspended).

Background scanners should be added alongside their phase as listed above. The generalized index reconciler and worker claim count reconciler should be added in Phase 1 or 2 as safety nets.

---

## 12. Rust Crate Architecture

### 12.1 Workspace Structure

```
flowfabric/
├── Cargo.toml                       # workspace
├── ferriskey/                       # Valkey client (our fork — shaped for FlowFabric)
├── lua/                             # split Lua source files → build.rs concatenates
│   ├── helpers.lua                  #   shared library-local helpers (ok/err/ok_duplicate/hgetall_to_table/etc.)
│   ├── version.lua                  #   ff_version (1 fn)
│   ├── execution.lua                #   ff_create/claim/complete/cancel/delay/move_to_waiting_children/fail/reclaim/expire (9 fns)
│   ├── lease.lua                    #   ff_renew_lease/mark_lease_expired_if_due/revoke_lease (3 fns)
│   ├── suspension.lua               #   ff_suspend/resume/create_pending_wp/expire_suspension/close_waitpoint (5 fns)
│   ├── signal.lua                   #   ff_deliver_signal/buffer_signal_for_pending_wp/claim_resumed_execution (3 fns)
│   ├── stream.lua                   #   ff_append_frame (1 fn)
│   ├── flow.lua                     #   ff_create_flow/add_execution_to_flow/cancel_flow/stage_dep_edge/apply_dep/resolve_dep/evaluate_eligibility/promote_blocked/replay (9 fns)
│   ├── budget.lua                   #   ff_create_budget/report_usage_and_check/reset_budget/unblock_execution/block_execution (5 fns)
│   ├── quota.lua                    #   ff_create_quota_policy/check_admission_and_record (2 fns)
│   └── scheduling.lua               #   ff_issue_claim_grant/change_priority/update_progress/promote_delayed/issue_reclaim_grant (5 fns)
├── crates/
│   ├── ff-core/                     #   types, partition math, key builders, error codes
│   ├── ff-script/                   #   ff_function! macro, typed FCALL wrappers, library loader
│   ├── ff-engine/                   #   cross-partition dispatch + scanner module
│   ├── ff-scheduler/                #   claim-grant cycle, fairness, capability matching, lanes
│   ├── ff-sdk/                      #   worker public API
│   ├── ff-test/                     #   integration test harness, fixtures, assertion helpers
│   └── ff-server/                   #   binary: config, gRPC/HTTP API, startup, metrics
```

### 12.2 Crate Responsibilities

**ferriskey** — Valkey client (our fork). Provides generic `fcall(name, keys, args) → Value`, `function_load_replace(source)`, `function_list(library)`. Cluster-aware routing via hash tag extraction (`{p:N}`, `{fp:N}`, `{b:M}`, `{q:K}`). Pipeline support for batching FCALLs across partitions. Connection pooling, TLS, topology refresh. We shape ferriskey to serve FlowFabric — if FCALL support needs improvement, we implement it here.

**ff-core** — Leaf crate, no Valkey dependency. State vector enums (`LifecyclePhase`, `OwnershipState`, `EligibilityState`, `BlockingReason`, `TerminalOutcome`, `AttemptState`). Domain ID types (`ExecutionId`, `LeaseId`, `LeaseEpoch`, `FlowId`, `BudgetId`, `WaitpointKey`). Partition math: `partition_for(entity_id, family, num_partitions) → partition_number`. Key builders: `exec_core_key(partition, eid) → "ff:exec:{p:3}:<eid>:core"`. All 40+ error codes from §10.7. Policy types (`RetryPolicy`, `TimeoutPolicy`, `ExecutionPolicy`). Validity matrix assertion helpers.

**ff-script** — Bridge between Rust types and Valkey Functions. Declarative `ff_function!` macro generates: KEYS array construction from typed args, ARGV marshaling, `FCALL` invocation via ferriskey, return parsing from `{1,"OK",...}/{0,"ERROR",...}` into typed `Result<T, ScriptError>`. One macro invocation per function (~43 total). Embeds Lua library source via `include_str!` (from build.rs concatenation output). Startup: calls `ferriskey::function_load_replace()` with version check via `FCALL ff_version`.

**ff-engine** — Cross-partition orchestration and background scanners. Partition router maps entity → partition → ferriskey connection. Cross-partition dispatchers: dependency resolution fan-out (§4.8e — complete on `{p:N}` → read edges from `{fp:N}` → resolve on each child's `{p:N}`), two-phase flow membership (`{p:N}` then `{fp:N}`), usage reporting (`{p:N}` then `{b:M}`). Scanner module (`ff_engine::scanner`): common `Scanner` trait with `scan_partition()` method, pipelined partition scanning, configurable intervals, metrics emission. All 14 scanner types from §6 implemented here (some with partition-scoped variants, including execution_deadline added in adversarial round 4). Used as a library by both `ff-sdk` (for cross-partition orchestrators) and `ff-server` (for scanners + dispatchers).

**ff-scheduler** — Claim-grant cycle: quota pre-check (read-only on `{q:K}`) → select candidate (weighted round-robin with deficit, capability matching) → per-candidate budget check on `{b:M}` (deny → block → next) → `ff_issue_claim_grant` on `{p:N}`. Lane management (4 states, registry). Fairness state (deficit counters, per-cycle budget cache). Depends on `ff-script + ff-core + ferriskey` only — does NOT depend on `ff-engine`. Embedded in `ff-server` for v1; crate boundary allows standalone extraction for horizontal scaling.

**ff-sdk** — Public API for worker authors. Clean surface: `connect()`, `claim_next()`, `complete()`, `fail()`, `suspend()`, `report_usage()`, `append_frame()`, `delay()`, `move_to_waiting_children()`. Auto lease renewal as background tokio task. Graceful shutdown (stop claims → delay active → wait). Idempotency key generation. Depends on `ff-engine` for cross-partition orchestrators (report_usage, suspend-and-wait). Simple single-partition operations (complete, fail, renew, append_frame) use `ff-script` wrappers directly.

**ff-test** — Integration test harness. Embedded Valkey via testcontainers. `FUNCTION LOAD REPLACE` fixture. Assertion helpers: `assert_execution_state!`, `assert_index_membership!`. Scenario builders for multi-step test flows. State vector completeness validator (after every state-mutating FCALL, verify all 7 dimensions). Version migration tests (load v1, create state, load v2, verify forward compatibility).

**ff-server** — Binary crate. Config loading (Valkey endpoints, partition counts, scanner intervals, TLS). Starts engine (14 scanners) + scheduler (embedded). 13 API methods: create_execution, cancel_execution, get_execution_state, get_execution (full HGETALL), deliver_signal (13K/17A), change_priority, replay_execution (variable KEYS), create_budget (variable ARGV), create_quota_policy, get_budget_status, create_flow, add_execution_to_flow, cancel_flow (+ cross-partition cancel dispatch). Graceful shutdown. Validates partition config against Valkey-stored `ff:config:partitions` on startup.

### 12.3 Dependency Graph

```
ferriskey                            (transport — generic Valkey client)
    ↑
ff-core                              (types — no Valkey dependency)
    ↑
ff-script                            (typed FCALL — ferriskey + ff-core)
    ↑
ff-engine                            (dispatch + scanners — ff-script)
    ↑                    ↑
ff-scheduler          ff-sdk          (scheduler: ff-script + ff-core + ferriskey only)
    ↑                    ↑            (sdk: ff-engine + ff-script + ff-core)
    └────── ff-server ───┘            (binary — imports all)
```

No circular dependencies. Single path from domain types to transport. ff-scheduler is intentionally isolated from ff-engine to allow standalone deployment.

### 12.4 Lua Library Build

Lua source files are split by domain for maintainability (~11 files, ~3000 lines total). A `build.rs` step concatenates them with a `#!lua name=flowfabric` preamble into a single `flowfabric.lua` blob. `helpers.lua` is always first (defines library-local functions). Order of subsequent files does not matter (all use `redis.register_function`). CI validates: load into real Valkey, run full integration suite.

**Cluster deployment:** `FUNCTION LOAD REPLACE` is per-shard in Valkey Cluster — not auto-replicated across shards. `ff-server` startup must iterate all shard primaries and `FUNCTION LOAD REPLACE` each one. `ferriskey` provides `cluster_function_load_all()` for this. Library size ~200KB × 48 shards ≈ 10MB total, ~1s startup time. During rolling library updates, some shards may temporarily have v(N) while others have v(N+1). Functions must be backward-compatible within a single version increment — ensured by the `or`-default pattern (§4.8m) for new fields and by never removing function names (deprecated functions return a structured error instead).

### 12.5 Testing Strategy

Three-layer testing. Lua functions are the spec — never mock them.

**Layer 1 — Lua integration tests** (authoritative): Per-function tests against real Valkey. Happy path + every error path. State vector completeness validation after every mutation. Cross-function scenarios (full claim→complete, suspend→signal→resume, reclaim, replay, cancel-from-each-state). Version migration tests. Includes tests for `ff_promote_blocked_to_eligible` (#35: zero-dep root member promotion) and `ff_close_waitpoint` (#36: worker cancels pending waitpoint without suspending).

**Layer 2 — Engine orchestration tests** (unit + integration): Trait-based `FcallExecutor` for unit tests of orchestration logic. Integration tests with real Valkey for end-to-end orchestration. **Scanner correctness tests:** For each scanner type, insert a specific desync state (e.g., execution with `eligibility_state=eligible_now` but missing from eligible ZSET; execution with expired lease but `ownership_state=leased`), run one scanner partition scan, verify the correction was made. Tests scanner idempotency (run twice = same result), discovery (finds all eligible items), and no false positives (doesn't act on healthy items). Reconciler tests: same pattern for budget, quota, dependency, and index reconcilers.

**Layer 3 — SDK + scanner end-to-end**: Full worker loop (register → claim → stream → report → complete). Full scanner cycle (create stale state → scan → verify cleanup). Budget breach scenario (report_usage → HARD_BREACH → auto fail). Suspend-and-wait round-trip (pending wp → signal → already_satisfied path AND normal resume path). Slow but catches integration bugs between layers.

**`ff-test` provides:** Embedded Valkey via testcontainers. `FUNCTION LOAD` fixture. Assertion helpers (`assert_execution_state!(exec_id, lifecycle=terminal, outcome=success)`, `assert_index_membership!(exec_id, index=eligible)`). Scenario builders (create + claim + complete in one call). Partition test config (4 execution, 2 flow, 2 budget, 2 quota — small but exercises cross-partition logic).

### 12.6 Phase 1 Crate Checklist

| Crate | Phase 1 Scope |
|-------|---------------|
| ferriskey | `fcall()`, `function_load_replace()`, cluster routing (likely already present) |
| ff-core | Core enums, ExecutionId, partition math, key builders, 15 error codes |
| ff-script | Library loader + 7 function wrappers + `ff_function!` macro |
| ff-engine | Minimal partition router, 3 scanners (lease expiry, delayed promoter, generalized reconciler) |
| ff-scheduler | Single-lane claim cycle (no fairness, no capability matching) |
| ff-sdk | `connect()`, `claim_next()`, `renew_lease()`, `complete()`, `cancel()` |
| ff-test | Valkey fixture, `FUNCTION LOAD`, `assert_execution_state!` |
| ff-server | Config, minimal gRPC (create, cancel), start engine+scheduler |

### 11.1 Testing Strategy

See §12.5 for the authoritative testing strategy (`ff-test` crate, three-layer model, scanner correctness tests).

---

## Addendum: Adversarial Review Discoveries

The following implementation constraints and improvements were found and fixed during 11 rounds of adversarial testing. Each item documents a production-affecting issue not covered by the original RFC text.

### A.1 Priority Clamp [0, 9000] (Round 4)

Composite eligible-ZSET scores use the formula `-(priority * 1_000_000_000_000) + created_at_ms`. Lua numbers are IEEE 754 doubles with a 53-bit mantissa, so priority values above 9007 cause integer overflow and score corruption. `ff_create_execution` and `ff_change_priority` clamp priority to the range [0, 9000] in Lua before computing the composite score. This is enforced at the Lua layer, not at the Rust type level, because the overflow is a Valkey/Lua numeric representation issue.

### A.2 Dedup Key TTL Guard (Round 6)

`SET key value PX 0` or `PX -1` causes a Valkey error inside a Lua function *after* partial writes have already committed, leaving the data store in an inconsistent state. All four dedup-key sites (`ff_create_execution`, `ff_deliver_signal`, `ff_buffer_signal_for_pending_waitpoint`, `ff_check_admission_and_record`) guard with `dedup_ms > 0` before issuing the `SET PX` command. When dedup_ms is zero or absent, the dedup key is not written.

### A.3 Quota Reconciler Self-Healing (Round 8)

Quota concurrency counters grow monotonically because `INCR` happens on admission (partition `{q:K}`) but there is no corresponding `DECR` when executions complete/fail/cancel (those run on partition `{p:N}`). The `quota_reconciler` scanner periodically SCANs `admitted:<eid>` guard keys (which have TTL = window_ms and auto-expire) and SETs the concurrency counter to the actual count of live guard keys. Drift corrects within one reconciliation cycle (~30s default).

### A.4 Execution Deadline Scanner (Round 4)

The 14th background scanner. Scans `execution_deadline_zset` for executions past their absolute deadline and calls `ff_expire_execution` for each. Unlike the lease-expiry scanner (which only handles active/leased executions), this scanner handles runnable-state executions (eligible, delayed, blocked) via dynamic key construction for index cleanup.

### A.5 Defensive Runnable-Path in ff_expire_execution (Round 5)

When `execution_deadline` fires on a runnable execution (one in eligible, delayed, or blocked state rather than active/leased), `ff_expire_execution` must ZREM from the correct source index. The function uses a defensive catch-all `else` branch that ZREMs from all 7 runnable-state index types (lane eligible, lane delayed, lane blocked, global equivalents, and priority override sets) for forward compatibility with new index types.

### A.6 SDK claim_resumed_execution Fallback (Round 8)

When `ff_claim_execution` returns the error code `use_claim_resumed_execution` (indicating the grant targets a suspended→resumed execution), the SDK `claim_next()` method falls back to calling `ff_claim_resumed_execution` with the execution_id from the grant. Without this fallback, resumed executions would be permanently unclaimed despite having valid grants in the ready queue.

### A.7 Claim Field Extraction Positions (Round 9)

`ff_claim_execution` returns `ok(lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type)` — six fields at positions 0–5. The SDK must read `field_str(4)` for `attempt_index`, not `field_str(2)` (which is `expires_at`, a 13-digit millisecond timestamp). A positional mismatch here is masked on first attempts (index 0, which happens to match `unwrap_or(0)` on parse failure) but breaks retry and reclaim paths. All SDK response parsers were audited for positional correctness.

### A.8 deliver_signal Effect Index (Phase C Review Round 2)

`ff_deliver_signal` returns `ok(signal_id, effect)` → `{1, "OK", signal_id, effect}`. Server `parse_deliver_signal_result` originally read the effect at array index 2 (which is signal_id), not index 3 (the actual effect string like `resume_condition_satisfied` or `appended_to_waitpoint`). This was masked because the test only checked `Accepted { .. }` without asserting the effect value. Fixed by reading `fcall_field_str(arr, 3)` and adding an assertion on the effect string.

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
