# RFC-020: Postgres Wave 9 — deferred backend impls

**Status:** DRAFT — Revision 2
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Revision 2:** 2026-04-26 — Round-1 reviewer findings addressed (schema-ground-truth, design-spine reframe, drop G6, full-parity + coherent-wave directive)
**Target release:** v0.11 (single coordinated ship; whole wave or withdraw)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction + staged Postgres parity), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions)
**Related artifacts:**
- `docs/POSTGRES_PARITY_MATRIX.md` — authoritative per-method status
- `crates/ff-backend-postgres/tests/capabilities.rs:53-73` — asserted false-flag list
- `crates/ff-backend-postgres/src/lib.rs` — current `EngineError::Unavailable` sites
- `crates/ff-backend-postgres/migrations/0001_initial.sql` — real schema (single source of truth for table/column names)
- Valkey reference impls in `crates/ff-backend-valkey/src/*`

> **Draft status:** this RFC consolidates scope + design-spine only. It
> is NOT accepted.
> Owner directive (2026-04-26): **full parity, coherent whole-wave ship,
> no MVP, no split across releases.** If any group cannot land cleanly
> the wave withdraws; we do not ship half.

---

## 1. Summary

The Postgres backend shipped behind the `EngineBackend` trait in v0.7
(RFC-017) and reached core hot-path parity with Valkey in v0.9. A
small, deliberately bounded set of trait methods — the **Wave 9**
surface — still returns `EngineError::Unavailable { op }` on Postgres.
Every one of them is admin-surface, read-model-shape, or new-table-
machinery work; none is on the create/dispatch/claim hot path.

Revision 2 reframes the design around **one shared spine applied
across operator-control-shaped methods**, plus two standalone groups
(budget admin, waitpoint listing), landing as **13 methods** (G6
dropped — see §3 and §8.4). The wave ships coherent in **one
release** (v0.11); sequencing below is **impl-effort order**, not a
staged consumer rollout.

---

## 2. Motivation

### 2.1 Why these were deferred

The RFC-017 staged migration (Stages A–E4) had a single forcing
constraint: ship Postgres as a real backend without regressing Valkey
semantics, on a session-bounded cadence. Every Stage boundary was a
CI gate. Wave 9 is the set of methods where at least one of the
following was true and the migration chose to ship a structured
`Unavailable` over a rushed impl:

1. **New table machinery required.** e.g. `cancel_flow_header` /
   `ack_cancel_member` need a cancel-backlog table; no analogue
   existed pre-RFC-017.
2. **Read-model shape was an open design fork.** e.g.
   `read_execution_info` / `read_execution_state` can be served
   either by a denormalised projection or by a join-on-read against
   `ff_exec_core` + `ff_attempt`. Picking one silently under
   Stage-E2 time pressure would have locked in the wrong answer.
3. **Admin-surface semantics are not hot-path.** The operator-control
   family (`cancel_execution`, `change_priority`, `replay_execution`,
   `revoke_lease`) and the budget/quota admin family are called at
   human latencies from cairn's operator UI; the cost of
   `Unavailable` here is bounded (visible 503 vs. silent wrong
   answer), and cairn consumes capabilities via RFC-018 so the UI
   greys out unsupported actions.

### 2.2 Why these need to land now — coherent

Owner directive: **full parity is the target**; "Postgres != parity"
is rejected (see §8.4). Consequences:

- The capability table's 13-entry `false` cluster is a single
  coherent gap, not independently-filable rows. The design spine
  (§3) makes the 13-to-1 collapse the point of this RFC.
- cairn currently pins at ff-server versions that still expose the
  gaps. A staged rollout (one method per minor) would stretch the
  pin window across five releases; a coherent v0.11 closes it in
  one.
- Matrix + capability-test updates are a single atomic capability-
  surface flip at release time (§6.3), not a drip-feed of flips.

---

## 3. Scope — one design spine + two standalone groups

Revision 2 reorganises the wave around **shared design primitives**
rather than table-shaped groups. The 13 methods split into:

- **Spine-A — operator-control SERIALIZABLE-fn + outbox emission**
  (9 methods spanning original G1 + G2 + G3): the methods that
  mutate execution/attempt/flow state and must emit RFC-019 outbox
  events where Valkey does. Share one SQL template (§4.2).
- **Spine-B — read-model join-on-read** (3 methods, original G2):
  the read-only trio sharing a read-model decision (§4.1 + §7.1).
  Grouped with Spine-A because the SERIALIZABLE-fn mutations
  project results through the same column set.
- **Standalone-1 — Budget / quota admin** (5 methods, original G4):
  sits on top of existing `budget.rs`; no cross-coupling to the
  spine.
- **Standalone-2 — `list_pending_waitpoints`** (1 method, original
  G5): partition-aware `SELECT` against `ff_waitpoint_pending`; no
  cross-coupling.

### 3.1 The 13 methods in scope

Every row is currently `EngineError::Unavailable` on Postgres and
every flag below is asserted `false` at
`crates/ff-backend-postgres/tests/capabilities.rs:53-73`.

**Spine-A (mutations, emit outbox):**

| Method | Valkey reference | Notes |
|---|---|---|
| `cancel_flow_header` | `ff_cancel_flow` FCALL + `AlreadyTerminal` idempotent replay | Needs cancel-backlog table (§4.3) |
| `ack_cancel_member`  | `ff_ack_cancel_member` = `SREM pending_cancels` + conditional `ZREM cancel_backlog` | Same backlog table |
| `cancel_execution`   | `HMGET` pre-read + `ff_cancel_execution` FCALL | emits `ff_lease_event` |
| `change_priority`    | `ff_change_priority` ff-script helper | emits `ff_signal_event` (see §4.2.4) |
| `replay_execution`   | `ff_replay_execution` + skipped-flow-member variant | see §4.2.5 — semantics called out |
| `revoke_lease`       | `ff_revoke_lease` + `AlreadySatisfied { reason }` | emits `ff_lease_event` |

**Spine-B (point reads):**

| Method | Valkey reference |
|---|---|
| `read_execution_info`   | `HGETALL exec_core` + StateVector parse |
| `read_execution_state`  | `HGET exec_core public_state` + JSON deserialize |
| `get_execution_result`  | `GET ctx.result()` (current attempt) |

**Standalone-1 — Budget / quota admin:**

| Method | Valkey reference |
|---|---|
| `create_budget`        | `ff_create_budget` FCALL |
| `reset_budget`         | `ff_reset_budget` FCALL |
| `create_quota_policy`  | `ff_create_quota_policy` FCALL |
| `get_budget_status`    | 3× HGETALL (definition + usage + limits) |
| `report_usage_admin`   | `ff_report_usage_and_check` without worker handle |

**Standalone-2 — Waitpoints:**

| Method | Valkey reference |
|---|---|
| `list_pending_waitpoints` | `SSCAN` + 2× `HMGET` (schema per Stage D1, HMAC-redacted) |

### 3.2 Dropped from scope: `subscribe_instance_tags`

Removed from Wave 9 entirely. It is `Unavailable` on **both** backends
(not a Postgres-only gap) and the #311 audit (2026-04-24) found no
concrete consumer demand; cairn's backfill use case is served by
`list_executions` + `ScannerFilter::with_instance_tag`. Wave 9 is
about closing the Postgres-specific capability cluster, not about
implementing speculative cross-backend methods.

See §5 Non-goals and §8.4.

### 3.3 Already-shipped (docs lag)

`rotate_waitpoint_hmac_secret_all` — ships today via
`signal::rotate_waitpoint_hmac_secret_all_impl`
(`lib.rs:840-853`). Capabilities test asserts `true`
(`capabilities.rs:44`). Housekeeping PR to flip the parity-matrix
row to `impl` is out-of-scope for this RFC (no parity-matrix edits
at draft time) but tracked as a release-time sweep item in §6.3.

---

## 4. Design sketches

Sketches fix SQL/outbox **shape**, not DDL details. Migration DDL
lands per-group in the implementing PR (§6.2). Shape decisions apply
across every method in the spine.

### 4.1 Spine-B — read model (3 methods)

**Decision: join-on-read against the real schema.** A denormalised
`ff_execution_view` projection was considered and rejected (§7.1
owner-resolve).

The three reads project from the same column set:

- `ff_exec_core (partition_key, execution_id, flow_id, lane_id,
  lifecycle_phase, ownership_state, eligibility_state, public_state,
  attempt_state, blocking_reason, cancellation_reason, cancelled_by,
  priority, created_at_ms, terminal_at_ms, deadline_at_ms, payload,
  result, policy, raw_fields)` — PRIMARY KEY `(partition_key,
  execution_id)`, HASH-partitioned 256 ways.
- `ff_attempt (partition_key, execution_id, attempt_index, ...,
  outcome, usage, policy, raw_fields)` — for StateVector attempt
  sub-states.

Shapes:

- `read_execution_state` = `SELECT public_state FROM ff_exec_core
  WHERE partition_key = $pk AND execution_id = $id`. Single-column
  point read, partition-local.
- `read_execution_info` = multi-column projection of `ff_exec_core`
  + `LEFT JOIN LATERAL` on `ff_attempt` for the current attempt row,
  mapping to StateVector. Partition-local (both tables co-located
  on `partition_key`, RFC-011).
- `get_execution_result` = **semantic decision**: matches Valkey's
  `GET ctx.result()` which returns the **current attempt's** result.
  Shape: `SELECT result FROM ff_exec_core WHERE partition_key = $pk
  AND execution_id = $id` — `ff_exec_core.result` already holds the
  current terminal attempt's result (see `0001_initial.sql:117`).
  Per-attempt history is out-of-scope; `attempt_index` is not an
  input. (See §7 for the open-question form — resolved here.)

No new table. All three reads are `ff_exec_core` queries; LATERAL
join for the composite case.

### 4.2 Spine-A — SERIALIZABLE-fn + outbox template (6 mutating methods)

All six mutating methods follow a single SQL template:

1. **Partition derivation.** Compute `partition_key` from the
   primary entity (execution_id or flow_id) using the same hash
   the existing backend uses (`execution_partition` /
   `flow_partition` equivalents in the postgres crate).
2. **`BEGIN ISOLATION LEVEL SERIALIZABLE`.**
3. **`SELECT ... FOR UPDATE`** on `ff_exec_core` + (where applicable)
   `ff_attempt` — captures pre-state and acquires row locks against
   the reconciler (§7.4).
4. **Validate** against Valkey-canonical semantics (e.g.
   `revoke_lease` returns `AlreadySatisfied { reason:
   "no_active_lease" }` when `ff_attempt.worker_instance_id IS
   NULL`; see `valkey/lib.rs:5858-5895`).
5. **Mutate** with explicit compare-and-set fencing: the `UPDATE`
   WHERE clause includes `attempt_index = $expected` and
   `ff_attempt.lease_epoch = $expected_epoch` (CAS on RFC-009
   fencing columns). Row-count = 0 → return `AlreadySatisfied` /
   `Conflict` per method contract, not error.
6. **Emit outbox row** in the same tx (§4.2.7). Trigger fires
   `pg_notify` at COMMIT; RFC-019 subscribers wake.
7. **COMMIT.** Retry loop on `40001` (serialization_failure)
   wrapped around steps 2–6, same pattern as the existing Stage-E3
   reconcilers.

#### 4.2.1 `cancel_execution`

- Pre-read: `SELECT lane_id, attempt_index, lifecycle_phase,
  worker_instance_id FROM ff_exec_core LEFT JOIN ff_attempt ... FOR
  UPDATE`.
- Mutate: set `lifecycle_phase = 'Cancelled'`, `cancellation_reason
  = $reason`, `cancelled_by = $source`, `terminal_at_ms = now()`.
  CAS on `attempt_index`.
- Outbox: `INSERT INTO ff_lease_event (execution_id, lease_id,
  event_type, occurred_at_ms, partition_key) VALUES (..., 'revoked',
  ...)` when `worker_instance_id IS NOT NULL` (lease was active);
  else no lease-event row.

#### 4.2.2 `revoke_lease`

- Pre-read: `SELECT worker_instance_id, lease_epoch FROM ff_attempt
  WHERE (partition_key, execution_id, attempt_index) = (...) FOR
  UPDATE`.
- If `worker_instance_id IS NULL` → return `AlreadySatisfied {
  reason: "no_active_lease" }` (Valkey-canonical; matches
  `valkey/lib.rs:5881-5892`). No mutation, no outbox row.
- Else: `UPDATE ff_attempt SET worker_instance_id = NULL,
  lease_expires_at_ms = NULL, lease_epoch = lease_epoch + 1 WHERE
  lease_epoch = $expected_epoch` (CAS). Row-count = 0 →
  `AlreadySatisfied { reason: "epoch_moved" }`.
- Outbox: `ff_lease_event` row with `event_type = 'revoked'`.

#### 4.2.3 `cancel_flow_header` + `ack_cancel_member`

- Mutate against the new `ff_cancel_backlog` table (§4.3).
- `cancel_flow_header`: `SELECT public_flow_state FROM ff_flow_core
  ... FOR UPDATE`; if already terminal → `AlreadyTerminal { policy,
  reason, members }` populated from the existing backlog row
  (idempotent replay); else INSERT backlog + bulk `UPDATE
  ff_exec_core SET public_state = 'Cancelling' WHERE (partition_key,
  flow_id) = ...`.
- `ack_cancel_member`: single `DELETE FROM ff_cancel_backlog_member
  WHERE (partition_key, flow_id, member_execution_id) = (...)`
  (junction-table shape — §7.5 resolved) + conditional DELETE of
  the parent backlog row when no members remain (CTE).
- Outbox: `ff_signal_event` row on header (flow-level), none on
  ack-member (too chatty; Valkey XADDs only the header event
  too — see `valkey/lib.rs:ff_cancel_flow`).

#### 4.2.4 `change_priority`

- Mutate: `UPDATE ff_exec_core SET priority = $new WHERE
  (partition_key, execution_id) = (...) AND lifecycle_phase IN
  ('Pending', 'Scheduled', 'Running') FOR UPDATE`. CAS on
  `lifecycle_phase` (not a priority change for terminal execs).
- Outbox: `ff_signal_event` row (new `event_type = 'priority_changed'`
  value; channel already exists from `0007_signal_event_outbox.sql`).
  Rationale: `change_priority` is not lease-affecting
  (`ff_lease_event` is wrong channel), but RFC-019 subscribers on
  `ff_signal_event` want to observe operator-driven priority
  mutations to re-drive scheduler projections. The existing
  `ff_signal_event` channel is the closest fit; alternative is a
  new `ff_operator_event` channel, which §7.3 considers and rejects
  for Wave 9 scope-creep reasons.

#### 4.2.5 `replay_execution` — semantic resolution

Valkey's `ff_replay_execution` is structurally distinct from a
"reset scheduler state" primitive: it performs an HMGET pre-read,
then either the base path (reset terminal state + increment
attempt_index) or the **skipped-flow-member** variant (SMEMBERS over
the flow's incoming-edge set, for flows where a member was skipped
rather than failed). See `valkey/lib.rs:5736-5850`.

Postgres parity:

- **Base path.** SERIALIZABLE tx:
  1. `SELECT lifecycle_phase, flow_id, attempt_index,
     outcome FROM ff_exec_core LEFT JOIN ff_attempt` FOR UPDATE.
  2. Require `lifecycle_phase` terminal (else `NotReplayable`).
  3. `UPDATE ff_exec_core SET lifecycle_phase = 'Pending',
     terminal_at_ms = NULL, result = NULL, attempt_index =
     attempt_index + 1, cancellation_reason = NULL, cancelled_by =
     NULL`.
  4. `INSERT INTO ff_attempt (partition_key, execution_id,
     attempt_index, ...)` for the new attempt row (initialised
     empty, like a fresh create). The prior terminal `ff_attempt`
     row stays — it's history, not state.
  5. Outbox: `ff_signal_event` row (`event_type = 'replayed'`).
- **Skipped-flow-member path.** Additional CTE over `ff_edge` (where
  `downstream_eid = $id`) to find incoming edges. Valkey's
  `SMEMBERS` maps to `SELECT DISTINCT upstream_eid FROM ff_edge
  WHERE (partition_key, flow_id, downstream_eid) = (...)`. The
  outbound shape — reset upstream-edge states so the skipped member
  becomes eligible again — matches the Valkey body.

This is **not** equivalent to "delete the result row" or "insert a
new `ff_attempt` and forget the old one." The ephemeral
scheduler-state reset in Valkey is structural state-machine replay;
Postgres implements it as the same state-machine transition,
persisted rather than ephemeral.

#### 4.2.6 Reconciler interaction (§7.4 resolved)

Spine-A methods race the `lease_expiry` reconciler
(`crates/ff-backend-postgres/src/reconcilers/lease_expiry.rs`).
Valkey atomicity comes from Lua; Postgres atomicity comes from
`SELECT ... FOR UPDATE` on `ff_attempt` + SERIALIZABLE isolation.
Specifically:

- `revoke_lease` + `lease_expiry` race → both acquire the attempt
  row's UPDATE lock; serialization error on the losing tx → retry
  loop observes the winner's state (either lease cleared by
  revoke, or cleared by reclaim). Idempotent on either path.
- `cancel_execution` + `lease_expiry` race → same mechanism.
  Cancel wins if commit-first; else reclaim's `lease_epoch` bump
  causes cancel's CAS to miss → retry observes reclaimed state
  and emits `ff_lease_event event_type='revoked'` with the
  already-cleared worker.
- `cancel_execution` vs `replay_execution` → both take UPDATE
  locks on `ff_exec_core`; serial. Whichever commits first wins.
  The loser's pre-state validation (requires terminal for replay,
  non-terminal for cancel) fails → method returns its method-
  specific `NotReplayable` / `AlreadyTerminal`. No corruption.

Net: the lock protocol + CAS fencing make reconciler-interaction
symmetric to Valkey's Lua-atomic behavior. No new reconciler
modifications required.

#### 4.2.7 Outbox emission matrix (§7.3 resolved: yes-emit)

| Method | Outbox | `event_type` |
|---|---|---|
| `cancel_execution` | `ff_lease_event` (if lease active) | `revoked` |
| `revoke_lease` | `ff_lease_event` | `revoked` |
| `change_priority` | `ff_signal_event` | `priority_changed` |
| `replay_execution` | `ff_signal_event` | `replayed` |
| `cancel_flow_header` | `ff_signal_event` | `flow_cancel_requested` |
| `ack_cancel_member` | (none — too chatty; matches Valkey) | — |

All emissions happen in the same SERIALIZABLE tx as the mutation,
flushed to subscribers via the existing `pg_notify` triggers
(`0006_lease_event_outbox.sql` / `0007_signal_event_outbox.sql`).
No new outbox tables required.

### 4.3 Cancel-backlog tables (partitioned) — Spine-A support

Corrects Round-1 finding #6 (top-level `ff_cancel_backlog` violates
the 256-way HASH-partition convention).

Two tables, both partitioned 256 ways on `partition_key`:

- `ff_cancel_backlog` — one row per flow, carries flow-level
  cancellation policy + reason + requested-at. Partition key:
  `partition_key` derived from `flow_id` via the existing
  `flow_partition` hash (RFC-011 co-location means flow + members
  share `partition_key`; backlog fits the same scheme). PK
  `(partition_key, flow_id)`.
- `ff_cancel_backlog_member` — junction table, one row per
  pending member. PK `(partition_key, flow_id, member_execution_id)`.
  §7.5 resolved to junction-table over `BYTEA[]` array-column
  because `ack_cancel_member` mutates a single element and array
  `array_remove` doesn't compose cleanly with partitioning +
  `FOR UPDATE` granularity.

Migration DDL lands in the Spine-A implementation PR (§6.2).

### 4.4 Standalone-1 — Budget / quota admin

Sits on top of the existing `budget.rs` module and its existing
tables (`ff_budget` / `ff_quota_policy` / budget-usage tables from
`0002_budget.sql`). Key observation (§7.2 resolved):
`report_usage_impl` at `crates/ff-backend-postgres/src/budget.rs:154`
already ships; its granularity choice is **already made** (see §7.2).
Wave 9 commits admin methods to match that shape.

- `create_budget` / `reset_budget` / `create_quota_policy` = single
  `INSERT ... ON CONFLICT DO UPDATE` — definitional writes against
  the existing schema.
- `get_budget_status` = 3 SELECTs (definition + usage + limits) +
  `NotFound { entity: "budget" }` on empty definition-row set.
  Mirrors Valkey's 3× HGETALL pattern.
- `report_usage_admin` = worker-handle-less variant of
  `report_usage`. Factor out the shared
  `report_usage_and_check_core` helper and call with a sentinel
  `worker_instance = None`. The existing `report_usage_impl`
  granularity (already shipping) is the binding contract.

### 4.5 Standalone-2 — `list_pending_waitpoints`

Real table is `ff_waitpoint_pending`
(`0001_initial.sql:298`), **not** `ff_waitpoint`. It is HASH-
partitioned 256 ways with PK `(partition_key, waitpoint_id)` and
has **no `status` column** — presence in the table implies pending
(signal-delivery moves rows out).

Columns actually present:
`partition_key, waitpoint_id, execution_id, token_kid, token,
created_at_ms, expires_at_ms, condition`.

`PendingWaitpointInfo` (trait return) requires `token_kid,
token_fingerprint, flow_id, execution_id, attempt_index`. Of those:

- `token_kid` — direct from `ff_waitpoint_pending`.
- `token_fingerprint` — computed from `token` at projection
  (redaction per Stage D1 contract; never returned raw).
- `execution_id` — direct.
- `flow_id` — requires JOIN to `ff_exec_core` on `(partition_key,
  execution_id)` (partition-local; co-located).
- `attempt_index` — requires JOIN to `ff_attempt` on
  `(partition_key, execution_id, attempt_index = current)`. Current
  attempt = `ff_exec_core.attempt_index`. Partition-local.

**Partition-enumeration.** The method does not take a partition key
as input; cursor `id > $after ORDER BY id LIMIT` is a cross-
partition scan. Shape:

- Pagination cursor is the lexicographic tuple `(partition_key,
  waitpoint_id)`; client opaque.
- Per-call: iterate partitions starting from the cursor's
  `partition_key`, emit rows ordered by `waitpoint_id` within each
  partition, advance partition when the inner cursor is exhausted
  or `LIMIT` is reached. Bounded cost: at worst touches 256
  partitions for a full-cluster scan; typical operator call with
  `LIMIT=100` touches one or two partitions before returning.
- Alternative (parallel-per-partition fanout) rejected for Wave 9:
  `list_pending_waitpoints` is human-latency operator surface, not
  a hot path; sequential iteration is simpler and matches Valkey's
  single-shard SSCAN shape.

Query sketch:

```sql
SELECT wp.partition_key,
       wp.waitpoint_id,
       wp.execution_id,
       wp.token_kid,
       wp.token,           -- fingerprinted in projection, never returned raw
       wp.created_at_ms,
       ec.flow_id,
       ec.attempt_index
FROM ff_waitpoint_pending wp
JOIN ff_exec_core ec
  ON ec.partition_key = wp.partition_key
 AND ec.execution_id  = wp.execution_id
WHERE (wp.partition_key, wp.waitpoint_id) > ($after_pk, $after_wp)
ORDER BY wp.partition_key, wp.waitpoint_id
LIMIT $limit;
```

`attempt_index` comes from `ff_exec_core.attempt_index` (current
attempt) — consistent with Valkey's single-attempt projection.

---

## 5. Non-goals

1. **Not re-litigating the trait surface.** RFC-012 locked the
   higher-op trait philosophy; RFC-017 locked the staged-migration
   shape; RFC-018 locked the capability-discovery shape. Wave 9 is
   implementation on top of those accepted decisions.
2. **Not changing Valkey behavior.** Every method translates
   existing Valkey-canonical semantics into Postgres SQL. Semantic
   ambiguities (e.g. `revoke_lease` `AlreadySatisfied`) are
   resolved explicitly in §4.2 as **match-Valkey**; the prior-round
   "match-Valkey or match-HTTP-404" fork is closed.
3. **Not introducing new consumer-facing APIs or HTTP response
   shapes.** The trait methods are already public; Wave 9 only
   changes the inner `Unavailable` to a real impl. No new crate-
   public types, no new HTTP routes, no new HTTP response-shape
   changes.
4. **Not changing the capability-flag surface.** Wave 9 only flips
   the `Supports` flags listed in §3.1 from `false` to `true` at
   release time (§6.3). No new fields in `Supports`, no changes to
   `capabilities.rs` other than the flips.
5. **Not implementing `subscribe_instance_tags`.** See §3.2 and
   §8.4. Wave 9 drops G6 entirely; the method stays
   `Unavailable` on both backends pending concrete consumer demand.
6. **Not a release gate for v0.10.** v0.10 already shipped
   (2026-04-26). Wave 9 ships in v0.11 (§6.3).

---

## 6. Sequencing + migration + release contract

Owner directive: **whole wave in one coordinated release**.
Sequencing below is implementation-effort order within one release
train; it is **not** a staged rollout.

### 6.1 Impl order

Reviewer-B's consumer-value argument (G2 is the biggest user-facing
gap; `examples/token-budget` regresses on Postgres for Standalone-1)
drove:

1. **Spine-B — read model (3 methods).** First because Spine-A
   mutations need the same column-projection shape for their
   response structs, and landing read shapes first de-risks the
   spine's `SELECT ... FOR UPDATE` templates.
2. **Standalone-1 — Budget / quota admin (5 methods).** No
   cross-coupling; unblocks `examples/token-budget` on Postgres.
3. **Spine-A pt.1 — `cancel_execution` + `revoke_lease` + `change_priority`
   (3 methods).** Existing tables only (`ff_exec_core`, `ff_attempt`,
   `ff_lease_event`, `ff_signal_event`); proves the SERIALIZABLE-fn
   + outbox template.
4. **Standalone-2 — `list_pending_waitpoints` (1 method).** Small,
   existing table, partition-cursor shakedown.
5. **Spine-A pt.2 — `cancel_flow_header` + `ack_cancel_member` +
   `replay_execution` (3 methods).** Introduces
   `ff_cancel_backlog` + `ff_cancel_backlog_member`; needs the
   spine template from step 3 in-tree.

Each step is its own PR; the release is cut **only after all five
steps merge**. A step that cannot land cleanly withdraws the wave
(owner directive: whole-wave-or-withdraw).

### 6.2 Migration contract (per-step PR, not stop-the-world)

DDL is consumer-facing in `FF_BACKEND=postgres`. Each step's PR
lands its own migration file (numbered sequentially after `0009_*`):

- **Forward-only.** New migration numbers; no edits to existing
  files.
- **Idempotent.** `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF
  NOT EXISTS` + `INSERT INTO ff_migration_annotation ... ON
  CONFLICT DO NOTHING`. Re-running `migrate()` is a no-op.
- **Additive.** No column drops, no type changes on existing
  columns; Wave 9 only adds `ff_cancel_backlog` +
  `ff_cancel_backlog_member`. No destructive DDL.
- **No stop-the-world.** `CREATE INDEX CONCURRENTLY` for any
  indexes added post-0001. Table creation + annotation inserts
  are cheap + locking-bounded.
- **backward_compatible = true** in every `ff_migration_annotation`
  row. Operators can roll backward to any pre-Wave-9 ff-server
  binary; extra tables sit idle.

### 6.3 Release + capability-flip protocol

- All 13 method impls + migrations merge to main behind their
  `Supports` flags still `false` (the trait impl is correct; the
  capability flag lies to consumers). **OR**, if the PR cleanly
  isolates flipping the flag, steps 1–5 each ship with their
  flag(s) flipped on-merge (both patterns acceptable per CI-gate
  discipline).
- **Final release PR** (no new method impls): flips any still-
  `false` `Supports` flags in §3.1 to `true`, updates
  `crates/ff-backend-postgres/tests/capabilities.rs:53-73` to
  assert `true` for the 13 flags, updates
  `docs/POSTGRES_PARITY_MATRIX.md` to drop the `stub` column for
  every method, appends v0.11 `CHANGELOG.md` section.
  **Also** flips the stale `rotate_waitpoint_hmac_secret_all`
  matrix row to `impl` as a housekeeping sweep (§3.3).
- Release gate per CLAUDE.md §5: smoke before tag, examples
  build, parity matrix current.

---

## 7. Open questions (resolved / to-owner)

Revision 2 resolves most Round-1 forks in-RFC (no new forks added).
Remaining items are flagged explicitly.

### 7.1 Read-model shape — **RESOLVED in §4.1**

Outcome: join-on-read against real schema, no `ff_execution_view`
projection. Cairn's call pattern is one-exec-at-a-time on operator
click; projection-table write amplification across every state
transition is not justified. See §4.1.

### 7.2 Budget counter granularity — **RESOLVED in §4.4**

`report_usage_impl` already ships
(`crates/ff-backend-postgres/src/budget.rs:154`). The granularity
choice is already made in-tree; admin methods commit to match that
shape. Not an open fork.

### 7.3 Operator-control outbox emission — **RESOLVED in §4.2.7**

Yes-emit for every mutating Spine-A method, routed to
`ff_lease_event` for lease-affecting ops and `ff_signal_event` for
others. Matrix in §4.2.7.

### 7.4 Reconciler interaction + cancel/replay race — **RESOLVED in §4.2.6**

`SELECT ... FOR UPDATE` + SERIALIZABLE isolation + CAS on fencing
columns give reconciler-symmetric atomicity. Detail in §4.2.6.

### 7.5 Cancel-backlog shape — **RESOLVED in §4.3**

Junction table (`ff_cancel_backlog_member`) over array-column
(`BYTEA[]`), both 256-way partitioned. Junction composes better
with partitioned `FOR UPDATE` row-level locking.

### 7.6 Capability-flip timing — **RESOLVED in §6.3**

Owner directive is coherent wave in one release. Flags flip in the
final release PR (or per-step PR if cleanly isolated); matrix flip
is atomic at release time.

### 7.7 — (withdrawn; `rotate_waitpoint_hmac_secret_all` housekeeping folded into §6.3)

### 7.8 `get_execution_result` attempt indexing — **RESOLVED in §4.1**

Matches Valkey's `GET ctx.result()` which is current-attempt.
`ff_exec_core.result` is the binding column; no `attempt_index`
input. Per-attempt-history is out of scope for Wave 9.

### 7.9 Remaining owner-call items (push if needed)

None. All §7 items resolved in Revision 2. If the owner disagrees
with any resolution (read-model join-on-read; junction-table
backlog; yes-emit outbox; get_execution_result=current-attempt),
§7 reopens the specific item in Round-3.

---

## 8. Alternatives rejected

### 8.1 Close-by-attrition (per-method issues, no unifying RFC)

Rejected: Spine-A shares one SQL template across 6 methods; splitting
into 6 issues loses the template-review surface and invites
inconsistent idiopathic implementations. The spine **is** the point
of the RFC; per-method issues regress on that.

### 8.2 Ship without RFC, per-method PRs

Rejected: 13 methods across two spine-shapes plus two standalones
is broad enough that scope creep is a real risk ("I'm already in
`budget.rs`, let me also refactor X" — the CHANGELOG-batch-conflict
shape that broke v0.3.x releases). An RFC with locked scope +
sequencing + non-goals prevents that drift.

### 8.3 Collapse Wave 9 into v0.10

Rejected: v0.10 already shipped (2026-04-26). Wave 9 is v0.11
scope. The capability-discovery reshape + `suspend_by_triple`
already used v0.10's budget; squeezing 13 additional methods
would have tripped the session-bounded cadence.

### 8.4 "Postgres != parity" — declare gaps permanent via RFC-018 capabilities

Considered seriously. The argument: RFC-018 exists precisely so
consumers can observe capability deltas; the 13 `false` flags are
already a supported state; cairn's UI already greys out unsupported
actions. Declaring the gaps permanent and deleting the methods
from Postgres would:

- Shrink Postgres maintenance surface.
- Eliminate the design-spine work this RFC proposes.
- Normalise "Postgres is a partial backend" as a first-class state.

**Rejected (owner call, 2026-04-26).** Reasoning:

1. "Partial backend" is a consumer-surface commitment we haven't
   signed up for. Every cairn release pins an ff-server version;
   a permanent-gap posture means cairn ships permanent branches on
   `capabilities.supports.cancel_execution` etc. That ossifies the
   capability surface into a product differentiator rather than a
   migration-state observable, which RFC-018 explicitly rejected
   (RFC-018 §Non-goals: "capabilities are not feature flags").
2. Operator-facing parity is the point. An operator who picks
   `FF_BACKEND=postgres` expects `cancel_execution` to work. A
   permanent `Unavailable` is a broken product, not a known gap.
3. The spine (§4.2) is not expensive to land. 6 methods share one
   template; 3 reads share one projection; 5 budget methods share
   existing `budget.rs`. Declaring permanent gaps to avoid ~2 weeks
   of implementation is poor trade.

The "partial backend" option stays open in principle for
genuinely-Valkey-native methods (the reason `subscribe_instance_tags`
is dropped per §3.2 is that it is a speculative both-backend
method, not a parity gap). For the Wave 9 13, full parity is the
target.

### 8.5 Gate on consumer demand — wait for concrete filings

Considered: hold Wave 9 until cairn files specific method asks.

**Rejected.** cairn is in mid-migration to `FF_BACKEND=postgres`;
asking cairn to file one issue per capability they discover broken
re-runs the peer-team-boundary churn from v0.3.2. The parity matrix
+ RFC-018 capabilities already give cairn full visibility;
demand-driven surfacing would just defer the work and spread the
pin window. Owner's full-parity directive overrides demand-gating.

### 8.6 Split schema-shape RFC from method-landing RFC

Considered: extract §4.1 read-model decision + §4.3 cancel-backlog
shape into a prerequisite "RFC-020a schema-shape" doc, with RFC-020
depending on it. Argument: schema decisions govern all future PG
read APIs, not just Wave 9.

**Rejected.** The schema decisions in Revision 2 are contained:
join-on-read against existing columns (no new projection tables);
one new backlog pair. Neither is a forward-facing primitive that
warrants a separate RFC audience. If a future Wave-10 introduces
projection tables or cross-cutting schema shapes, it is welcome to
split at that time. RFC-020 stays single-doc.

### 8.7 Split Wave 9 across v0.11 + v0.12

Considered: land Spine-B + Standalone-1 in v0.11; Spine-A +
Standalone-2 in v0.12. Argument: smaller per-release risk surface;
reviewer-C's "ship-G4+G5-only" variant.

**Rejected (owner call).** Coherent ship is the full-parity
promise; splitting means cairn pins across two minors on an
incomplete capability set. Whole-wave-or-withdraw: the split-RFC
shape is explicitly the failure mode the directive is avoiding.

---

## 9. References + consumer-ask posture

- `docs/POSTGRES_PARITY_MATRIX.md` — authoritative per-method
  status, v0.8.0 parity summary table rows
- `crates/ff-backend-postgres/tests/capabilities.rs:53-73` —
  asserted false-flag list, the Wave-9 scope contract (post-§3.2
  G6 removal, still 13 methods)
- `crates/ff-backend-postgres/migrations/0001_initial.sql` — real
  schema (`ff_exec_core`, `ff_attempt`, `ff_waitpoint_pending`,
  `ff_flow_core`, `ff_edge`); all references in §4 are traceable
- `crates/ff-backend-postgres/migrations/0006_lease_event_outbox.sql`
- `crates/ff-backend-postgres/migrations/0007_signal_event_outbox.sql`
- `crates/ff-backend-postgres/src/lib.rs` — current `Unavailable`
  sites (grep `EngineError::Unavailable`)
- `crates/ff-backend-postgres/src/budget.rs:154` — shipped
  `report_usage_impl` (the §4.4 / §7.2 binding shape)
- `crates/ff-backend-postgres/src/reconcilers/lease_expiry.rs` —
  the §4.2.6 race partner
- `crates/ff-backend-valkey/src/lib.rs:5570-5900` — Valkey
  reference impls for Spine-A semantic parity
- `rfcs/RFC-012-engine-backend-trait.md` §Round-7 — higher-op
  trait philosophy
- `rfcs/RFC-017-ff-server-backend-abstraction.md` §9 — staged-
  migration structure, Stage E4 Wave-9 deferral notes
- `rfcs/RFC-018-backend-capability-discovery.md` §4 — `Supports`
  struct shape
- `rfcs/RFC-019-stream-cursor-subscription.md` — outbox +
  `LISTEN/NOTIFY` pattern (basis for §4.2.7 outbox emission)
- Issue #311 — `subscribe_instance_tags` deferral audit (the basis
  for the §3.2 drop)
- Issue #277 — cairn operator-UI capability-discovery driver

### 9.1 Consumer-ask posture — honest disclosure

Issues #277 and #335 establish cairn's capability-discovery wiring
and operator-UI greying for Wave-9 methods. As of 2026-04-26 cairn
has **not** filed per-method asks for Spine-A's `change_priority` /
`replay_execution` / Standalone-1's `create_quota_policy`.
`read_execution_info` / `read_execution_state` have been
tangentially cited in cairn operator-UI milestones; no formal ask.
`list_pending_waitpoints` is pulled by the RFC-013/014 suspension
tooling in cairn, but not an open issue.

The full-parity directive (2026-04-26) overrides consumer-demand
gating: Wave 9 ships the 13 methods because the backend claims
parity, not because every method has a filed consumer ask. Peer-team
boundary discipline holds — cairn does not modify ff-server, and
ff-server does not modify cairn; the parity ship simply unblocks
pin-version advancement when cairn is ready.
