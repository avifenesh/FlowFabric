# RFC-020: Postgres Wave 9 — deferred backend impls

**Status:** ACCEPTED — Revision 8
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Accepted:** 2026-04-26
**Revision 8:** 2026-04-26 — Wave 9 follow-up findings (#354, #355, #356) landed. **(#354)** §4.1 "maps directly" claim for `ff_exec_core.lifecycle_phase` + sibling state columns corrected to acknowledge the richer private alphabet encoded at the column level (Postgres writes `cancelled`, `released`, `pending_claim`, bare `running`, `blocked` as legitimate literals); the `normalise_*` shim in `crates/ff-backend-postgres/src/exec_core.rs` is documented as the boundary adapter, not a transient inconsistency — Option B (bless the adapter) per owner decision. **(#355)** `ff_attempt.outcome` now cleared on the two cancel paths that previously left it stale (`flow.rs` cancel-member loop; `exec_core::cancel_impl`); the existing `operator::cancel_execution_once` path already cleared it on live leases. **(#356)** Additive migration 0016 promotes first-claim timestamp from a LATERAL join on `ff_attempt.started_at_ms` to a typed `ff_exec_core.started_at_ms` column, set-once on first claim; Spine-B read path drops the LATERAL join. Parallel SQLite migration. Scope grows by one additive migration (0016 — 0015 reserved for RFC-024 claim-grant table) on each backend; method count unchanged.
**Revision 2:** 2026-04-26 — Round-1 reviewer findings addressed (schema-ground-truth, design-spine reframe, drop G6, full-parity + coherent-wave directive)
**Revision 3:** 2026-04-26 — Round-2 reviewer findings addressed: (A1/A2) new `ff_operator_event` channel (migration 0010) instead of repurposing `ff_signal_event`; (A3) `PendingWaitpointInfo` contract aligned, additive migration 0011 adds `waitpoint_key`/`state`/`required_signal_names`/`activated_at_ms` (**Revision 5 correction: `waitpoint_key` already shipped via migration 0004; 0011 adds 3 columns, not 4**); (A4) reconciler per-attempt scoping audited; (A5) `outcome`/`lifecycle_phase` column-binding clarified; (B1–B4) release-PR line items tightened; (C1) per-release-risk engagement in §8.7; (C2) intra-ack race traced to SERIALIZABLE retry; (C3a–d) resolved in §4.2.4/§4.2.7/§4.1/§6.3. Scope grows by two additive migrations (0010 + 0011); method count unchanged at 13.
**Revision 4:** 2026-04-26 — Round-3 reviewer-A findings addressed: (NEW-1) `lifecycle_phase` enum literals corrected to lowercase real-enum values (`cancelled` / `runnable` / `NOT IN ('terminal','cancelled')`) across §4.2.1, §4.2.4, §4.2.5, §4.2.6; (NEW-4) stray `FOR UPDATE` removed from UPDATE in §4.2.4; (NEW-5) `waitpoint_key` pre-0011 projection behavior pinned in §4.5 (COALESCE to `''` + degraded-row counter) — **superseded by Revision 5 (moot: column already exists from 0004)**; (NEW-2) SERIALIZABLE retry budget pinned to 3 (matches `CANCEL_FLOW_MAX_ATTEMPTS` in `flow.rs:52`) in §4.2.3; (B-1) scope note for the producer-side `suspend_*` write-path change added to §3; (B-2) rollback-forward-only caveat added to §6.2; (C-polish) §8 adds a one-liner pointing at §4.2.4 for the repurpose-`ff_signal_event` rejection. No scope, alternatives, or new forks re-opened.
**Revision 5:** 2026-04-26 — Ground-truth correction: `waitpoint_key` column already exists on `ff_waitpoint_pending` since migration 0004 (Wave 4d), populated as `TEXT NOT NULL DEFAULT ''` and actively used by `suspend_ops.rs`. Revision 4's §4.5 treated it as a new 0011 column; NEW-5 degraded-counter design was predicated on pre-0011 NULL rows that can never exist. This revision reframes migration 0011 as 3 additive columns (`state`, `required_signal_names`, `activated_at_ms`), drops the `waitpoint_key` addition, drops NEW-5 degraded-counter, simplifies §4.5 query to read `waitpoint_key` directly. No scope or method-count change.
**Revision 7:** 2026-04-26 — Spine-A pt.2 impl-attempt surfaced three semantic forks in §4.2.5 + §4.2.4 that Revision 6 wrote against absent Postgres structures. Resolved in Revision 7 against the §5.2 "no Valkey behavior change" mandate: (Fork 1, §4.2.5 skipped-flow-member path) Valkey's per-dep-edge state reset + deps_meta counter recompute (`flowfabric.lua:8557-8615`) has no column on `ff_edge_group` (`ff_dep_edge` + `ff_deps_meta` are Valkey-only). **Resolved Option A** — reset downstream's `ff_edge_group` counters to neutral state (`running_count = 0`, `skip_count = 0`, `fail_count = 0`; `success_count` preserved) via UPDATE driven by `SELECT DISTINCT downstream_eid FROM ff_edge WHERE downstream_eid = $id` — structurally closest to Valkey's dep-state reset; feasibility confirmed against `ff_edge_group` schema (`0001_initial.sql:217-230`, no CHECK constraints on counter columns). (Fork 2, §4.2.5 normal-replay path) Revisions 4-6 bumped `attempt_index + 1` + INSERT new `ff_attempt`; Valkey (`flowfabric.lua:8625-8650`) mutates in-place without bumping. **Resolved Option A** — in-place UPDATE on the existing `ff_attempt` row, no `attempt_index` bump, matching Valkey exactly. §4.2.6 per-attempt-scope audit (A4) narrowed accordingly — no historical-rows invariant any more. (Fork 3, §4.2.4 `change_priority` return shape) Revision 6 §4.2.4 specified `AlreadyTerminal` on row-count=0 but `ChangePriorityResult` contract (`crates/ff-core/src/contracts/mod.rs:445-448`) has only a `Changed` variant. **Resolved Option C → mirror Valkey** — Valkey's `ff_change_priority` (`flowfabric.lua:3683-3688`) returns `err("execution_not_eligible")` on any non-runnable / non-eligible state, which is an `EngineError` in the Rust layer, not an outcome-enum variant. Postgres matches: row-count=0 maps to `EngineError::ExecutionNotEligible` (or equivalent mapping of Valkey's `execution_not_eligible` error), not a new `AlreadyTerminal` variant. No contract amendment needed; `ChangePriorityResult::Changed` remains the sole variant. No scope, method count, migration, or non-§4.2.4/§4.2.5/§4.2.6 content change.

**Revision 6:** 2026-04-26 — Standalone-1 (Budget/quota admin) §4.4 rework. Standalone-1 impl attempt surfaced four structural gaps that Revision 5's §4.4 glossed over ("sits on top of existing `0002_budget.sql` tables" was false for quota; `BudgetStatus`'s 9 runtime fields have no column source on `ff_budget_policy`; budget reset scheduling has no Postgres analogue for Valkey's `budget_resets_zset` + `BudgetResetScanner`; and the "3× HGETALL → 3 SELECTs" query sketch was written against a schema that does not exist). Revision 6 resolves: (Gap 1) new **migration 0012 `ff_quota_policy` family** — 3 tables (`ff_quota_policy`, `ff_quota_window`, `ff_quota_admitted`) with concurrency represented as a column on `ff_quota_policy` (single Valkey counter-key = single Postgres column; `quota_policies_index` collapses to a partition-scoped SELECT, no dedicated table), 256-way HASH-partitioned on `partition_key`; (Gap 2) new **migration 0013** adds `next_reset_at_ms BIGINT` + 4 breach-tracking columns + 3 definitional columns to `ff_budget_policy` (Option A: single-table; breach updates remain READ COMMITTED to match the existing `report_usage_impl` isolation per Q11 — SERIALIZABLE retry budget does not apply on this hot path), and adds a new Postgres-native `BudgetResetReconciler` in `crates/ff-backend-postgres/src/reconcilers/budget_reset.rs` that selects due resets by `next_reset_at_ms <= now` (Option B for scheduling — column-on-policy over separate schedule table); (Gap 3) the 9 `BudgetStatus` fields (`breach_count`, `soft_breach_count`, `last_breach_at_ms`, `last_breach_dim`, `next_reset_at_ms`, `scope_type`, `scope_id`, `enforcement_mode`, `created_at_ms`) are columns on `ff_budget_policy` via migration 0013; `report_usage_impl` is amended to maintain `breach_count` / `soft_breach_count` / `last_breach_at_ms` / `last_breach_dim` incrementally matching Valkey's `flowfabric.lua:6576-6580,6614` pattern (this lifts Revision 5's §7.2 "don't touch shipped `report_usage_impl`" pin for this specific extension — see §7.2); (Gap 4) §4.4 query sketches rewritten against the real 2-table shape (`ff_budget_policy` including counters + `ff_budget_usage`), with `hard_limits` / `soft_limits` parsed from `policy_json`. Scope grows by two additive migrations (0012, 0013) and one new scanner (`budget_reset` Postgres reconciler); method count unchanged at 13. No spine-A, spine-B, standalone-2, release-gate, or non-budget §6.1 change.
**Target release:** v0.11 (single coordinated ship; whole wave or withdraw)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction + staged Postgres parity), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions)
**Related artifacts:**
- `docs/POSTGRES_PARITY_MATRIX.md` — authoritative per-method status
- `crates/ff-backend-postgres/tests/capabilities.rs:53-73` — asserted false-flag list
- `crates/ff-backend-postgres/src/lib.rs` — current `EngineError::Unavailable` sites
- `crates/ff-backend-postgres/migrations/0001_initial.sql` — real schema (single source of truth for table/column names)
- Valkey reference impls in `crates/ff-backend-valkey/src/*`

> **Status:** ACCEPTED (Revision 6; Revisions 2-5 history above).
> Owner directive (2026-04-26): **full parity, coherent whole-wave ship,
> no MVP, no split across releases.** If any group cannot land cleanly
> the wave withdraws; we do not ship half. Revision 6 narrows to
> §4.4 Standalone-1 (budget/quota admin); no scope change elsewhere.

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

Revision 3 adds two additive migrations to the wave:

- **0010 `ff_operator_event` outbox** — new channel for operator-
  control events (`priority_changed` / `replayed` /
  `flow_cancel_requested`). Preserves the RFC-019 `ff_signal_event`
  subscriber contract (§4.2.7, §4.3). Option X per Round-2 §central
  decision.
- **0011 `ff_waitpoint_pending` additive columns** — `state`,
  `required_signal_names`, `activated_at_ms`. Required to serve the
  real `PendingWaitpointInfo` contract (§4.5). (`waitpoint_key`
  already exists from migration 0004 and is actively written by
  `suspend_ops.rs` — not added here. Corrected in Revision 5.)

Both migrations are prerequisites for the claimed method-impls (not
new features) and ship in the Wave-9 release-PR sequence (§6.2).

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
  surface flip at the release PR (§6.3), not a drip-feed of flips.
  (Revision 3 pins this; Revision 2's fallback "or per-step if
  cleanly isolated" is removed — §6.3 is now unambiguous.)

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
| `change_priority`    | `ff_change_priority` ff-script helper | emits `ff_operator_event` (new channel, §4.2.4) |
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

### 3.1.1 Producer-side write-path change (not a 14th method)

Standalone-2 (`list_pending_waitpoints`) also modifies the producer-
side `suspend_*` write path to populate the 3 new 0011 columns
(`state`, `required_signal_names`, `activated_at_ms`) on initial
insert + activation transitions. (`waitpoint_key` is already written
on insert by `suspend_ops.rs` since migration 0004 and needs no new
wiring here.) On initial insert the producer writes
`required_signal_names` from the condition spec; `state` defaults
to `'pending'` and `activated_at_ms` is NULL until the
pending→active activation transition, which writes
`state = 'active'` + `activated_at_ms = now()`. This is in-scope
change surface outside the 13 listed methods but is **not** a new
method impl — it is the minimum necessary write-side wiring to let
Standalone-2's read-path serve the real `PendingWaitpointInfo`
contract (§4.5). Wired in Spine-A pt.2's step-4 PR alongside
migration 0011 (§6.2 step 4).

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

**Column-literal alphabet (Revision 8 correction, closes #354).** The
Postgres `ff_exec_core` state columns (`lifecycle_phase`,
`ownership_state`, `eligibility_state`, `public_state`,
`attempt_state`) encode `(phase × eligibility × terminal-outcome)` in
a **richer private alphabet** than the canonical `LifecyclePhase` /
`OwnershipState` / `EligibilityState` / `PublicState` / `AttemptState`
enums in `ff_core::state`. Earlier revisions described this mapping as
"maps directly"; that claim was wrong. Write-paths in this crate
(`flow.rs:674` cancel-member, `suspend_ops.rs` suspension-resume
claim, `exec_core.rs` initial insert, scheduler-transitional claim
paths) legitimately write Postgres-specific literals such as
`cancelled`, `released`, `pending_claim`, bare `running`, `blocked`
that are not members of the public enums. Read-paths call through a
normalisation shim (`normalise_lifecycle_phase`,
`normalise_ownership_state`, `normalise_eligibility_state`,
`normalise_public_state`, `normalise_attempt_state` in
`crates/ff-backend-postgres/src/exec_core.rs`) that collapses the
column literals to the closest public-enum variant before
`serde_json::from_str` deserialises into the enum type. **This
adapter-at-the-read-boundary is the documented architecture, not a
transient shim.** Option A (migrating the write-paths to canonical
literals) was considered and rejected: it would relocate the
encoding of terminal-outcome + scheduler-transitional state from SQL
columns into per-write computation, which does not simplify the
codebase — it just moves the mapping layer and loses the column-level
audit trail of which backend phase produced each row. New read paths
against these columns MUST call through the `normalise_*` helpers;
new write paths MAY introduce new column literals, but MUST update
the corresponding `normalise_*` arm in the same PR.

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

- Pre-read: `SELECT ec.lane_id, ec.attempt_index, ec.lifecycle_phase,
  a.worker_instance_id FROM ff_exec_core ec LEFT JOIN ff_attempt a
  ON (a.partition_key, a.execution_id, a.attempt_index) =
  (ec.partition_key, ec.execution_id, ec.attempt_index) FOR UPDATE`.
  Column binding: `lane_id` / `attempt_index` / `lifecycle_phase`
  live on `ff_exec_core`; `worker_instance_id` / `outcome` /
  `lease_epoch` live on `ff_attempt` (matches A5).
- Mutate: set `lifecycle_phase = 'cancelled'`, `cancellation_reason
  = $reason`, `cancelled_by = $source`, `terminal_at_ms = now()`.
  CAS on `attempt_index`. (Lowercase `cancelled` matches the real
  enum; see `crates/ff-backend-postgres/src/flow.rs:674`.)
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
- Outbox: `ff_operator_event` row on header (flow-level,
  `event_type='flow_cancel_requested'`), none on ack-member
  (too chatty; Valkey XADDs only the header event too — see
  `valkey/lib.rs:ff_cancel_flow`).

**Intra-ack race (C2).** Two concurrent `ack_cancel_member` calls on
the same `(partition_key, flow_id)` both execute the conditional
parent-DELETE CTE (`WITH deleted_member AS (DELETE ...) DELETE FROM
ff_cancel_backlog WHERE flow_id = $flow AND NOT EXISTS (SELECT 1
FROM ff_cancel_backlog_member WHERE ...)`). Under SERIALIZABLE, the
two transactions observe identical snapshots of the predicate-read
on `ff_cancel_backlog_member`; one commits first, the second
surfaces a `40001 serialization_failure` at COMMIT. The retry loop
(step 7 of the §4.2 template) re-runs the tx with the committed
state visible: the losing tx's member-DELETE becomes a no-op (row
already gone), the parent-DELETE predicate re-evaluates against
post-winner state, and the method returns idempotent success. The
retry budget is **3 attempts**, matching the existing
`CANCEL_FLOW_MAX_ATTEMPTS` constant
(`crates/ff-backend-postgres/src/flow.rs:52`); exhaustion surfaces
`ContentionKind::RetryExhausted` to the caller, who falls back on
the reconciler. No new retry-budget surface is introduced.

#### 4.2.4 `change_priority`

- Pre-read: `SELECT lifecycle_phase FROM ff_exec_core WHERE
  (partition_key, execution_id) = (...) FOR UPDATE`. Acquires the
  row lock against the reconciler per the §4.2 step-3 template.
- Validate: `lifecycle_phase NOT IN ('terminal','cancelled')`
  (non-terminal gate; terminal execs cannot have priority changed).
  Lowercase values match the real `lifecycle_phase` enum
  (`pending | blocked | eligible | active | leased | suspended |
  runnable | terminal | cancelled`); see dispatch.rs:356 for the
  same idiom.
- Mutate: `UPDATE ff_exec_core SET priority = $new WHERE
  (partition_key, execution_id) = (...) AND lifecycle_phase NOT IN
  ('terminal','cancelled')`. Row-count = 0 (concurrent terminal
  transition after pre-read, or pre-read already saw terminal) →
  return the Valkey-mirroring error `EngineError` mapping of
  `execution_not_eligible`, **not** a new `ChangePriorityResult`
  enum variant. Valkey's `ff_change_priority`
  (`crates/ff-script/src/flowfabric.lua:3683-3688`) fails-closed with
  `err("execution_not_eligible")` whenever `lifecycle_phase ~=
  "runnable"` or `eligibility_state ~= "eligible_now"`; this is the
  existing `ff-script` error mapping (`crates/ff-script/src/error.rs:218`,
  `error.rs:663`) surfaced to callers as an `EngineError` — not an
  outcome-enum variant. Postgres must mirror that shape per §5.2
  (no Valkey behavior change). `ChangePriorityResult` stays
  single-variant (`Changed { execution_id }`); no contract amendment.
  (Revision 4: Revision 3's draft had a stray `FOR UPDATE` on the
  `UPDATE`, which is invalid SQL — `FOR UPDATE` is SELECT-only.
  The row-lock comes from the pre-read SELECT above per the §4.2
  template.)
  (Revision 7: Revision 6's "row-count = 0 → return `AlreadyTerminal`"
  was written against a contract variant that does not exist
  (`crates/ff-core/src/contracts/mod.rs:445-448` has only `Changed`).
  Fork 3 resolved Option C → mirror Valkey's
  `execution_not_eligible` error instead of minting a new variant.)
- Outbox: `ff_operator_event` row (migration 0010, §4.3) with
  `event_type = 'priority_changed'`.

**Channel-choice rationale (revised in Revision 3).** Revision 2
routed operator-control events onto `ff_signal_event` on the grounds
that the channel "already exists." Round-2 review (Reviewer A, A1 +
A2) exposed two load-bearing problems with that shape:

1. **Schema mismatch.** `ff_signal_event` has no `event_type`
   column; its columns are `(event_id, execution_id, signal_id,
   waitpoint_id, source_identity, delivered_at_ms, partition_key)`
   plus `(namespace, instance_tag)` from 0009. Emitting a
   `priority_changed` row onto it requires either a schema-breaking
   migration on `signal_id NOT NULL` or a `DEFAULT 'synthetic'`
   kludge — both worse than a dedicated table.
2. **Subscriber-contract fork.** `0007_signal_event_outbox.sql`
   header comment binds the channel to "signal-delivery" semantics.
   RFC-019 subscribers treat the stream as signal-delivery outbox;
   injecting operator-control events silently changes the
   subscriber contract (passive subscribers drink operator events
   as if they were signal deliveries).

Revision 3 picks Option X: a new `ff_operator_event` outbox table +
`LISTEN/NOTIFY` channel (DDL in §4.3, migration 0010). Clean
channel separation preserves RFC-019 signal-delivery semantics;
subscribers opt in to operator events explicitly.

Revision 2's "no new outbox tables required" claim is withdrawn.
The Round-2 rejection-rationale paragraph below is kept as the
historical record but tagged overridden.

#### 4.2.5 `replay_execution` — semantic resolution

Valkey's `ff_replay_execution` is structurally distinct from a
"reset scheduler state" primitive: it performs an HMGET pre-read,
then either the base path (reset terminal state + increment
attempt_index) or the **skipped-flow-member** variant (SMEMBERS over
the flow's incoming-edge set, for flows where a member was skipped
rather than failed). See `valkey/lib.rs:5736-5850`.

Postgres parity:

- **Base path (normal replay).** SERIALIZABLE tx:
  1. `SELECT ec.lifecycle_phase, ec.flow_id, ec.attempt_index,
     ec.terminal_outcome, a.outcome FROM ff_exec_core ec LEFT JOIN
     ff_attempt a ON (a.partition_key, a.execution_id, a.attempt_index)
     = (ec.partition_key, ec.execution_id, ec.attempt_index)
     FOR UPDATE`.
     Column binding (A5): `lifecycle_phase` / `flow_id` /
     `attempt_index` / `terminal_outcome` live on `ff_exec_core`;
     `outcome` lives on `ff_attempt`. The join pins to the
     current-attempt row only.
  2. Require `lifecycle_phase = 'terminal'` (else `NotReplayable`).
  3. `UPDATE ff_exec_core SET lifecycle_phase = 'runnable',
     terminal_at_ms = NULL, result = NULL, cancellation_reason =
     NULL, cancelled_by = NULL, terminal_outcome = NULL`.
     **`attempt_index` is NOT incremented** (Revision 7 Fork 2,
     Option A). The post-replay phase is `'runnable'` (not
     `'pending'`) to match Valkey's `ff_replay_execution` body,
     which writes `"lifecycle_phase", "runnable"` on the base
     normal-replay path (`crates/ff-script/src/flowfabric.lua:8625`)
     and on the skipped-flow-member path (same file, line 8591).
     `'pending'` in the real enum denotes a freshly-created
     execution pre-dependency-resolution — a different state than
     post-replay reset. (Skipped-flow-member variant may
     additionally transition through blocked/eligibility sub-states
     via the edge-group reset; those are scheduler secondary-state
     columns orthogonal to `lifecycle_phase`.)
  4. `UPDATE ff_attempt SET outcome = NULL, completed_at_ms = NULL,
     worker_id = NULL, lease_id = NULL, lease_epoch = 0,
     lease_expires_at_ms = NULL, result = NULL, error_code = NULL,
     error_message = NULL WHERE (partition_key, execution_id,
     attempt_index) = (ec.partition_key, ec.execution_id,
     ec.attempt_index)`. **In-place mutation** of the existing
     current-attempt row — **no INSERT of a new `ff_attempt` row,
     no historical row retained** (Revision 7 Fork 2, Option A).
     This matches Valkey's `ff_replay_execution` which mutates
     `exec_core` in place and does not bump
     `current_attempt_index`. The prior terminal attempt's state
     is overwritten, not archived — same observable behavior as
     Valkey. Consumers observing
     `get_execution_info().current_attempt_index` see the same
     index across backends before and after replay.
  5. Outbox: `ff_operator_event` row (`event_type = 'replayed'`,
     `details` JSON carries `{"variant": "normal"}`).

- **Skipped-flow-member path.** Triggered when the terminal
  `ff_exec_core` row has `terminal_outcome = 'skipped'` AND
  `flow_id IS NOT NULL`. In addition to the base-path mutations
  (steps 3 + 4 above, minus step 2's `'terminal'` gate — the gate
  holds here too because Valkey also requires terminal state before
  replay), reset downstream edge-group counters so the replayed
  exec can re-enter the dependency-gated flow.

  **Valkey ground truth** (`flowfabric.lua:8557-8615`): iterate
  each incoming `dep_edge`, flip `impossible → unsatisfied` edges,
  SADD to `deps_unresolved`, HSET `deps_meta.unsatisfied_required_count
  = <new-count>`, HSET `deps_meta.impossible_required_count = 0`,
  flip `exec_core` → `lifecycle_phase='runnable'` +
  `eligibility_state='blocked_by_dependencies'`.

  **Postgres translation (Revision 7 Fork 1, Option A).** Postgres
  has no `ff_dep_edge` or `ff_deps_meta` table; dependency gating
  is represented by `ff_edge_group` counters
  (`0001_initial.sql:217-230`, columns `success_count`,
  `fail_count`, `skip_count`, `running_count`, invariant
  `total = success + fail + skip + running` maintained by
  `crates/ff-backend-postgres/src/dispatch.rs:300-339` +
  `edge_cancel_dispatcher.rs:234`). Reset the downstream's edge
  groups to a neutral state so upstream re-execution transitions
  drive the counters back up via the normal dispatch-path code
  (dispatch.rs:304 decrements `running` then increments the
  terminal bucket — the normal lifecycle handles the rebuild).

  Concrete SQL (inside the same SERIALIZABLE tx as base-path
  steps 3 + 4):
  ```sql
  -- Fork 1 Option A: reset downstream edge-group counters.
  -- skip_count / fail_count / running_count zeroed; success_count
  -- preserved (upstreams that already succeeded stay accounted;
  -- they do not re-run on replay of the downstream, so their
  -- contribution is stable).
  UPDATE ff_edge_group
     SET skip_count    = 0,
         fail_count    = 0,
         running_count = 0
   WHERE (partition_key, flow_id, downstream_eid) IN (
     SELECT DISTINCT e.partition_key, e.flow_id, e.downstream_eid
       FROM ff_edge e
      WHERE e.partition_key = $partition
        AND e.downstream_eid = $execution_id
   );
  ```
  (The SELECT is degenerate — `ff_edge_group` is keyed on
  `(partition_key, flow_id, downstream_eid)` and the replayed
  execution IS the downstream, so there is exactly one matching
  edge-group row per `(flow_id, downstream_eid)` pair. The
  `SELECT DISTINCT` form is kept for symmetry with
  `dispatch.rs:320` and tolerates multi-group edges if
  downstream fan-in later grows.)

  Additionally, override step 3's `lifecycle_phase = 'runnable'`
  secondary state: mirror Valkey's
  `eligibility_state = 'blocked_by_dependencies'` semantic by
  leaving the downstream out of any eligible-promotion path —
  with `skip_count = 0` and zero upstream terminal signals, the
  existing scanner promotion gate (`dispatch.rs` evaluate →
  `Decision::Satisfied` only when all groups hit k_target) will
  not promote this exec until upstream re-executions drive the
  counters back up. No separate `eligibility_state` column write
  is needed — the gate is counter-driven.

  **Why Option A over alternatives.** Option B (delete + recreate
  `ff_edge_group` rows) loses `policy` + `k_target` + `policy_version`
  and forces a re-read of the graph's edge policy, adding a JOIN to
  `ff_edge` per replay; Option C (mark exec runnable but leave
  counters untouched) leaves `skip_count > 0` which the
  dispatch-path evaluate() treats as a terminal signal, so the
  replayed exec stays eligible-but-never-promoted — behaviorally
  broken; Option D (return `EngineError::Unavailable` for
  skipped-flow-member replay) is honest but violates the §5.2
  full-parity mandate and leaves cairn's replay UI partially
  broken on Postgres. Option A's `success_count` preservation is
  justified by Valkey's mirror semantic: Valkey leaves
  `state='satisfied'` edges untouched during the skip-replay
  reset (`flowfabric.lua:8580` comment "satisfied edges remain
  satisfied (upstream already succeeded)").

  **Double-bump analysis.** Concern: upstream re-execution's
  terminal transition hits `dispatch.rs:304` which
  `running -= 1` — decrementing a counter we just set to 0 →
  clamped to 0 via `running.max(0)` (dispatch.rs:336). Then
  the outcome bucket (`success_count` / `fail_count` /
  `skip_count`) increments normally. No double-bump: the
  counter lifecycle is symmetric — we reset to a state as-if no
  upstream had yet terminalized, and normal lifecycle drives
  counters up from there.

  Outbox: `ff_operator_event` row (`event_type = 'replayed'`,
  `details` JSON carries `{"variant": "skipped_flow_member",
  "groups_reset": N}` where N is the row-count returned by the
  UPDATE above).

This is **not** equivalent to "delete the result row" or "insert a
new `ff_attempt` and forget the old one." The ephemeral
scheduler-state reset in Valkey is structural state-machine replay;
Postgres implements it as the same state-machine transition,
persisted rather than ephemeral. Revision 7 further tightens: the
in-place UPDATE on `ff_attempt` (base path) and the counter reset
on `ff_edge_group` (skipped-flow-member path) both preserve the
current `attempt_index`, matching Valkey's behavior exactly —
replayed execs keep the same `current_attempt_index` across
backends.

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
  The loser's pre-state validation (replay requires
  `lifecycle_phase = 'terminal'`; cancel requires
  `lifecycle_phase NOT IN ('terminal','cancelled')`) fails →
  method returns its method-specific `NotReplayable` /
  `AlreadyTerminal`. No corruption.

Net: the lock protocol + CAS fencing make reconciler-interaction
symmetric to Valkey's Lua-atomic behavior. No new reconciler
modifications required.

**Per-attempt scope audit (A4).** Revision 7 note: the historical-
rows hazard flagged here is **no longer applicable** under
Revision 7's Fork-2 Option-A resolution in §4.2.5 — `replay_execution`
now mutates the current `ff_attempt` row in place and does **not**
bump `attempt_index`, so there is no N-row history accumulation on
`ff_attempt`. The audit below is preserved verbatim as the historical
reasoning record (it cleared the now-retracted N-row-history shape
against the same scanners) and demonstrates that the scanners are
also robust under the in-place-mutate shape: every scanner keys
against `ff_exec_core.attempt_index` / `lifecycle_phase`, so
whether `ff_attempt` carries one row or many, the scoping is
correct. Revision 7 content:

- `reconcilers/lease_expiry.rs:60-83` scans `ff_attempt` rows with
  non-NULL `lease_expires_at_ms`. Historical rows have
  `lease_expires_at_ms = NULL` after their terminal transition
  (lease is released at terminal), so they are not selected.
  `release_one` keys by explicit `(partition_key, execution_id,
  attempt_index)` triple — no cross-attempt contamination.
- `reconcilers/attempt_timeout.rs:50-77` scans `ff_attempt` JOINed
  against `ff_exec_core` on `lifecycle_phase = 'active'`. A
  historical attempt whose `ff_exec_core` is now `'runnable'`
  (post-replay) is not `'active'` and is filtered out. `expire_one`
  (line 131-153) explicitly re-reads `ff_exec_core.attempt_index`
  as `cur_attempt` and binds the UPDATE `WHERE attempt_index = $4`
  to the scanner's picked index — if that doesn't match the current
  attempt, the row update is a no-op on historical data. (Note:
  `lifecycle_phase = 'active'` — lowercase — matches the real enum
  in `reconcilers/attempt_timeout.rs:58`.)
- `reconcilers/edge_cancel_reconciler.rs` / `dependency.rs` /
  `edge_cancel_dispatcher.rs` operate on `ff_exec_core` +
  `ff_edge*` — no direct `ff_attempt` scan surface.
- `reconcilers/suspension_timeout.rs` operates on
  `ff_suspension_current`; orthogonal.

Conclusion: the existing scanner surface already scopes by
`ff_exec_core.attempt_index` (directly or transitively via
`lifecycle_phase`), so adding historical `ff_attempt` rows is
invariant-safe. No new scanner modifications required.

#### 4.2.7 Outbox emission matrix (§7.3 resolved: yes-emit, Option X in Revision 3)

| Method | Outbox | `event_type` |
|---|---|---|
| `cancel_execution` | `ff_lease_event` (if lease active) | `revoked` |
| `revoke_lease` | `ff_lease_event` | `revoked` |
| `change_priority` | `ff_operator_event` (new, migration 0010) | `priority_changed` |
| `replay_execution` | `ff_operator_event` | `replayed` (details: `{variant: "normal" \| "skipped_flow_member", groups_reset?: N}` — Revision 7 §4.2.5) |
| `cancel_flow_header` | `ff_operator_event` | `flow_cancel_requested` |
| `ack_cancel_member` | (none — too chatty; matches Valkey) | — |

All emissions happen in the same SERIALIZABLE tx as the mutation,
flushed to subscribers via the existing `pg_notify` trigger on
`ff_lease_event` (`0006_lease_event_outbox.sql`) plus the new
`ff_operator_event` trigger (migration 0010, §4.3).

**Subscriber-contract analysis (A2).** `ff_signal_event` (RFC-019
Stage B, `0007_signal_event_outbox.sql`) is bound by header comment
to "signal-delivery" semantics: producers INSERT one row per
successful `deliver_signal` call. Its schema (`event_id,
execution_id, signal_id NOT NULL, waitpoint_id, source_identity,
delivered_at_ms, partition_key`) + instance-tag columns added by
0009 reflects that shape. Operator-control events do not have a
`signal_id`; repurposing the channel would either force
`signal_id = synthetic` rows (breaking existing subscribers that
treat `signal_id` as lookup-ready) or require a schema-breaking
migration to `signal_id NULL`. Revision 3 avoids both by introducing
the dedicated `ff_operator_event` channel (§4.3). Existing RFC-019
subscribers on `ff_signal_event` see no contract change.

The "no new outbox tables required" claim from Revision 2 is
withdrawn. Migration 0010 is an additive prerequisite, not a new
feature (§6.2).

### 4.3 New tables — Spine-A support

Revision 3 introduces two additive tables. Migrations ship in the
Spine-A implementation PRs (§6.2).

#### 4.3.1 `ff_operator_event` outbox (migration 0010)

New outbox table mirroring `ff_signal_event`
(`0007_signal_event_outbox.sql`) + `ff_lease_event`
(`0006_lease_event_outbox.sql`) shape, dedicated to operator-control
events. DDL shape:

```sql
CREATE TABLE ff_operator_event (
    event_id        BIGSERIAL PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL,  -- 'priority_changed' | 'replayed' | 'flow_cancel_requested'
    details         jsonb,          -- method-specific payload (new_priority, flow_id, etc.)
    occurred_at_ms  BIGINT NOT NULL,
    partition_key   INT NOT NULL,
    namespace       TEXT,           -- RFC-019 subscriber-filter (parity with 0009)
    instance_tag    TEXT            -- RFC-019 subscriber-filter
);

CREATE INDEX ff_operator_event_partition_key_idx
    ON ff_operator_event (partition_key, event_id);

CREATE FUNCTION ff_notify_operator_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('ff_operator_event', NEW.event_id::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER ff_operator_event_notify_trg
    AFTER INSERT ON ff_operator_event
    FOR EACH ROW EXECUTE FUNCTION ff_notify_operator_event();
```

`namespace` / `instance_tag` ship in the initial DDL (rather than
bolted on in a later 0009-shaped migration) because Wave 9's
consumer surface already expects RFC-019 subscriber filters to work.

Catch-up shape + subscription integration mirrors `ff_signal_event`;
RFC-019's subscription infrastructure gains a third channel with no
structural changes.

#### 4.3.2 Cancel-backlog tables (partitioned)

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

### 4.4 Standalone-1 — Budget / quota admin (Revision 6 rework)

**Revision 6 reframe.** Revision 5 asserted Standalone-1 "sits on
top of existing `0002_budget.sql` tables" and treated the 5 admin
methods as thin INSERT/SELECT wrappers. Ground truth after the
Standalone-1 impl attempt:

- `0002_budget.sql` ships **only** `ff_budget_policy` +
  `ff_budget_usage` + `ff_budget_usage_dedup`. **There is no
  `ff_quota_policy`, no `ff_quota_window`, no concurrency counter,
  no admitted-set.** `create_quota_policy` has no schema to write
  against.
- `ff_budget_policy` has columns `(partition_key, budget_id,
  policy_json, created_at_ms, updated_at_ms)` — only 5 columns.
  `BudgetStatus` (`contracts/mod.rs:1413-1430`) requires 9 fields
  not present: `breach_count`, `soft_breach_count`,
  `last_breach_at`, `last_breach_dim`, `next_reset_at`,
  `scope_type`, `scope_id`, `enforcement_mode`, `created_at`.
  (`scope_type` / `scope_id` / `enforcement_mode` / `created_at`
  could be derived by parsing `policy_json`, but that's fragile +
  couples read-path to policy-blob shape.)
- Valkey has `budget_resets_zset` scored on `next_reset_at_ms` +
  a dedicated scanner (`crates/ff-engine/src/scanner/budget_reset.rs`)
  that ZRANGEBYSCOREs due budgets and FCALLs `ff_reset_budget` per
  partition. Postgres has **no equivalent** — `reset_budget` works
  as a manual admin FCALL but periodic resets don't fire.

Revision 6 adds two additive migrations (§4.4.5) and extends
`report_usage_impl` to maintain breach counters (§4.4.6). The
spine/standalone decomposition is unchanged.

#### 4.4.1 `create_quota_policy` — schema prerequisite (migration 0012)

Valkey's `ff_create_quota_policy` (see `flowfabric.lua:6839-6878`)
writes 5 keys per policy on `{q:K}`:

| Valkey key | Shape | Postgres target |
|---|---|---|
| `ff:quota:{q:K}:<qid>` | Hash: `quota_policy_id`, `requests_per_window_seconds`, `max_requests_per_window`, `active_concurrency_cap`, `created_at` | Row in `ff_quota_policy` |
| `ff:quota:{q:K}:<qid>:window:<dim>` | ZSET (sliding-window request timestamps) | Row per (policy, dimension, admission-ts) in `ff_quota_window` |
| `ff:quota:{q:K}:<qid>:concurrency` | Counter (INCR/DECR; int-as-string) | Column `active_concurrency` on `ff_quota_policy` — single-counter per policy mirrors Valkey's single-key shape |
| `ff:quota:{q:K}:<qid>:admitted_set` | SET of admitted `execution_id`s | Row per (policy, execution_id) in `ff_quota_admitted` |
| `ff:idx:{q:K}:quota_policies` | SET of policy IDs on this partition (reconciler discovery replacing SCAN) | Implicit — `SELECT quota_policy_id FROM ff_quota_policy WHERE partition_key = $pk` replaces the SCAN-avoidance SET; no dedicated table (§7.12 rationale) |

Concurrency counter on the policy row: §4.4.4 audits contention (one
admit/release vs. one config read) and concludes a column on the
policy row is fine given admission granularity. If contention
surfaces post-ship we split to a separate `ff_quota_concurrency`
table; see §7.12.

**DDL shape** (migration `0012_quota_policy.sql`, spec follows
`0002_budget.sql` conventions — `-- no-transaction`, HASH 256 on
`partition_key`, explicit COMMIT between partition-child DO blocks,
`backward_compatible=true`):

```sql
-- Policy definition (one row per policy).
CREATE TABLE ff_quota_policy (
    partition_key                 smallint NOT NULL,
    quota_policy_id               text     NOT NULL,
    requests_per_window_seconds   bigint   NOT NULL,
    max_requests_per_window       bigint   NOT NULL,
    active_concurrency_cap        bigint   NOT NULL,
    active_concurrency            bigint   NOT NULL DEFAULT 0,
    created_at_ms                 bigint   NOT NULL,
    updated_at_ms                 bigint   NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id)
) PARTITION BY HASH (partition_key);

-- Sliding-window admission record. One row per admitted request per
-- (policy, dimension). ZREMRANGEBYSCORE maps to DELETE WHERE
-- admitted_at_ms < now - window_ms.
CREATE TABLE ff_quota_window (
    partition_key     smallint NOT NULL,
    quota_policy_id   text     NOT NULL,
    dimension         text     NOT NULL DEFAULT '',
    admitted_at_ms    bigint   NOT NULL,
    execution_id      text     NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, dimension, admitted_at_ms, execution_id)
) PARTITION BY HASH (partition_key);

-- No separate sweep index: the PK is `(partition_key, quota_policy_id,
-- dimension, admitted_at_ms, execution_id)`, which already covers
-- sweep scans as a B-tree prefix match on the first four columns.
-- Reviewer finding (gemini-code-assist, PR #350): redundant index
-- dropped to reduce write amplification.

-- Idempotent admitted-set: presence = admitted-within-current-window.
-- Mirrors Valkey's SADD admitted_set. `expires_at_ms` bounds the row
-- for the admitted-guard TTL semantic (Valkey's
-- `admitted_guard_key` SET NX PX window_ms) — sweeper trims expired
-- rows.
CREATE TABLE ff_quota_admitted (
    partition_key     smallint NOT NULL,
    quota_policy_id   text     NOT NULL,
    execution_id      text     NOT NULL,
    admitted_at_ms    bigint   NOT NULL,
    expires_at_ms     bigint   NOT NULL,
    PRIMARY KEY (partition_key, quota_policy_id, execution_id)
) PARTITION BY HASH (partition_key);

CREATE INDEX ff_quota_admitted_expires_idx
    ON ff_quota_admitted (partition_key, expires_at_ms);

-- 3 parents × 256 children = 768 partition children, created in
-- three `DO` blocks with explicit COMMIT boundaries per the 0002
-- pattern.
```

`ff_quota_concurrency` (enumerated in Revision 6 pre-draft as a
fourth table) is **not** a separate table — the concurrency counter
is a column on `ff_quota_policy.active_concurrency` per the table
above. "4 Postgres tables mirroring 5 Valkey keys" with the 5th
being implicit (SELECT over `ff_quota_policy` for the partition)
and one Valkey key collapsed onto a column (`concurrency`).

**`create_quota_policy` query shape:**

```sql
INSERT INTO ff_quota_policy
    (partition_key, quota_policy_id, requests_per_window_seconds,
     max_requests_per_window, active_concurrency_cap,
     active_concurrency, created_at_ms, updated_at_ms)
VALUES ($1, $2, $3, $4, $5, 0, $6, $6)
ON CONFLICT (partition_key, quota_policy_id) DO NOTHING
RETURNING created_at_ms;
```

Row-count = 0 → return `CreateQuotaPolicyResult::AlreadySatisfied`
(matches Valkey's `ok_already_satisfied`); row-count = 1 →
`Created`.

#### 4.4.2 `create_budget` — definitional write against existing `ff_budget_policy`

Migration 0013 (§4.4.5) extends `ff_budget_policy` with the
breach / scheduling / definitional columns. `create_budget`'s
INSERT populates them:

```sql
INSERT INTO ff_budget_policy
    (partition_key, budget_id, policy_json, scope_type, scope_id,
     enforcement_mode, breach_count, soft_breach_count,
     last_breach_at_ms, last_breach_dim, next_reset_at_ms,
     created_at_ms, updated_at_ms)
VALUES ($1, $2, $3, $4, $5, $6, 0, 0, NULL, NULL,
        CASE WHEN $7::bigint > 0 THEN $8::bigint + $7::bigint ELSE NULL END,
        $8, $8)
ON CONFLICT (partition_key, budget_id) DO NOTHING
RETURNING created_at_ms;
```

`$7` is `reset_interval_ms`; when > 0, `next_reset_at_ms` is seeded
to `now + interval` so the `budget_reset` reconciler picks it up.
Matches Valkey's `ff_create_budget` `ZADD resets_zset` scheduling
(flowfabric.lua:6522-6526).

Row-count = 0 → `CreateBudgetResult::AlreadySatisfied`; row-count = 1
→ `Created`.

#### 4.4.3 `reset_budget` — manual admin FCALL + scheduler-compatible

SERIALIZABLE tx (matches Valkey's Lua atomicity — the zero-all
pattern must not see mid-flight increments from concurrent
`report_usage_admin`):

```sql
-- Zero all usage rows for this budget.
UPDATE ff_budget_usage
SET current_value = 0, last_reset_at_ms = $3, updated_at_ms = $3
WHERE partition_key = $1 AND budget_id = $2;

-- Reset metadata + reschedule.
UPDATE ff_budget_policy
SET last_breach_at_ms = NULL,
    last_breach_dim   = NULL,
    updated_at_ms     = $3,
    next_reset_at_ms  = CASE
        WHEN (policy_json->>'reset_interval_ms')::bigint > 0
        THEN $3 + (policy_json->>'reset_interval_ms')::bigint
        ELSE NULL
    END
WHERE partition_key = $1 AND budget_id = $2
RETURNING next_reset_at_ms;
```

Returns `ResetBudgetResult::Reset { next_reset_at_ms }`. If the
row doesn't exist → `EngineError::NotFound { entity: "budget" }`
(matches Valkey's no-op when the Hash is missing).

**Reset scanner (Gap 2, Option B).** Valkey's scheduler-side
`BudgetResetScanner` lives in `crates/ff-engine/src/scanner/budget_reset.rs`
and is Valkey-native (ZRANGEBYSCORE + FCALL). Postgres adds a new
Postgres-native reconciler in
`crates/ff-backend-postgres/src/reconcilers/budget_reset.rs`
(named `budget_reset` matching the Valkey scanner-name so
observability metrics align across backends; wired into
`reconcilers/mod.rs::register_all`). Cadence matches the existing
`BudgetResetScanner::new(interval)` passed from
`ff-server::config::budget_reset_interval` (server.rs:395) — the
ff-server config knob drives both backends. Scan shape:

```sql
-- Per partition: pick due resets, bounded batch.
SELECT budget_id
FROM ff_budget_policy
WHERE partition_key = $1
  AND next_reset_at_ms IS NOT NULL
  AND next_reset_at_ms <= $2
ORDER BY next_reset_at_ms
LIMIT $3;
```

Batch size matches existing scanners (20; see
`crates/ff-backend-postgres/src/reconcilers/lease_expiry.rs` +
`crates/ff-engine/src/scanner/budget_reset.rs:18`). For each
returned `budget_id` the reconciler invokes the same
`reset_budget_impl` codepath the admin FCALL uses — one
SERIALIZABLE tx per budget (same retry budget as §4.2.3). Races
between a manual `reset_budget` + the scanner's reset resolve
identically to Valkey: both observe the same
`next_reset_at_ms`-recompute + RETURNING, so the second commit is
idempotent (re-zero-all is a no-op; next_reset_at_ms overwritten
with the later `now + interval`, scanner picks it up next tick).

#### 4.4.4 `create_quota_policy` / admission interaction (reference, not in-scope for 13-method Wave 9)

`ff_check_admission_and_record` (the hot-path admission FCALL,
Valkey `flowfabric.lua:6892-6974`) is **not** a Wave-9 method; it
lives on the hot path and ships pre-Wave-9 on Valkey. Migration
0012's schema is scoped to what `create_quota_policy` needs to
persist + what future admission impls need to reference; admission
itself is a separate backlog item, and the hot-path tx shape
(SELECT FOR UPDATE on `ff_quota_policy` + INSERT on
`ff_quota_window` + INSERT on `ff_quota_admitted` + concurrency
INCR as a column UPDATE) is sketched here for schema-shape
review only. Wave 9 ships the schema + `create_quota_policy` only;
the admission impl is a follow-up on another wave.

**Concurrency-column contention check (§7.12).** `active_concurrency`
is a single column on `ff_quota_policy`; each admission/release
UPDATEs `SET active_concurrency = active_concurrency + 1` (or -1)
under `FOR UPDATE` row lock. If admission RPS per policy exceeds
tens-of-thousands/sec this becomes a contention hotspot; at that
point splitting to a separate `ff_quota_concurrency` table with
sharded-counter shape is the migration. Wave 9 accepts the
single-column shape because (a) admission is not Wave-9 scope
(definition-write only), (b) Valkey's single-counter shape is the
same semantics, and (c) the split migration is additive. Tracked
as §7.12 open question.

#### 4.4.5 Migrations 0012 + 0013 (additive)

- **0012 `0012_quota_policy.sql`** — Gap 1. DDL in §4.4.1. Creates
  `ff_quota_policy` + `ff_quota_window` + `ff_quota_admitted` (3
  tables, 256-way HASH-partitioned) + indexes.
  `backward_compatible=true`. Lands in the Standalone-1
  implementation PR per §6.2.
- **0013 `0013_budget_policy_extensions.sql`** — Gaps 2+3.
  Additive ALTER on `ff_budget_policy`:

```sql
ALTER TABLE ff_budget_policy
    ADD COLUMN scope_type          text   NOT NULL DEFAULT '',
    ADD COLUMN scope_id            text   NOT NULL DEFAULT '',
    ADD COLUMN enforcement_mode    text   NOT NULL DEFAULT '',
    ADD COLUMN breach_count        bigint NOT NULL DEFAULT 0,
    ADD COLUMN soft_breach_count   bigint NOT NULL DEFAULT 0,
    ADD COLUMN last_breach_at_ms   bigint,
    ADD COLUMN last_breach_dim     text,
    ADD COLUMN next_reset_at_ms    bigint;

CREATE INDEX CONCURRENTLY ff_budget_policy_reset_due_idx
    ON ff_budget_policy (partition_key, next_reset_at_ms)
    WHERE next_reset_at_ms IS NOT NULL;
```

`backward_compatible=true` — all adds are nullable-or-defaulted.
Existing rows get `scope_type=''`, `breach_count=0`,
`next_reset_at_ms=NULL`. Wave 9's Standalone-1 impl populates the
columns on `create_budget` writes post-0013; pre-0013-inserted rows
return `scope_type=''` etc. in `get_budget_status` — degraded but
non-breaking (matches the usual "new column on old rows" posture).
The partial index `ff_budget_policy_reset_due_idx` backs the reset
scanner query (§4.4.3) — partial because most budgets have
`next_reset_at_ms=NULL` (no scheduled reset). The `CONCURRENTLY`
qualifier is required (§6.2's no-stop-the-world rule) and forces
the `CREATE INDEX` statement out of the `-- no-transaction`
migration's `DO`-block sequence — it runs as its own top-level
statement after the partition-child COMMITs, identical to the
existing 0001 pattern for non-DDL-batched indexes.

`ALTER TABLE ... ADD COLUMN` with a defaulted NOT NULL column
performs a table-rewrite on Postgres < 11; with the project's
Postgres 16 floor (`.github/workflows/matrix.yml:300` pins
`postgres:16-alpine` as the CI Postgres image), the default is
stored in catalog metadata and the ALTER is O(1)-metadata-only.
Partition-child rewrite is not needed.

Migration numbering follows Wave 9's impl-effort order: 0010
(operator_event) + 0011 (waitpoint-extensions) ship in spine-A pt.1
and standalone-2 respectively; 0012 + 0013 ship in Standalone-1's
PR. 0012 precedes 0013 because the quota schema is fresh (no prior
rows) while 0013 ALTERs a populated table; ordering them
0012-then-0013 mirrors "new table first, existing-table extension
second" as a readability convention, not a hard dependency.

#### 4.4.6 `report_usage_admin` + breach-counter maintenance (Gap 3)

Revision 5's §7.2 pinned `report_usage_impl` as "already shipping,
don't touch" to avoid hot-path scope creep. Revision 6 lifts the
pin **for the breach-counter update specifically** and keeps it in
place for every other aspect (READ COMMITTED isolation, FOR
UPDATE per-dim row locking, dedup handling, policy parsing). The
amendment is the narrowest possible extension:

- On **hard breach** (current code at `budget.rs:287-302`): before
  committing the `HardBreach` outcome, run
  ```sql
  UPDATE ff_budget_policy
  SET breach_count = breach_count + 1,
      last_breach_at_ms = $ts,
      last_breach_dim   = $dim,
      updated_at_ms     = $ts
  WHERE partition_key = $pk AND budget_id = $bid;
  ```
  Matches Valkey's `HINCRBY breach_count 1` + `HSET
  last_breach_at / last_breach_dim` at `flowfabric.lua:6576-6580`.
- On **soft breach** (current code at `budget.rs:326-336`): after
  the per-dim increments commit, increment `soft_breach_count`:
  ```sql
  UPDATE ff_budget_policy
  SET soft_breach_count = soft_breach_count + 1,
      updated_at_ms     = $ts
  WHERE partition_key = $pk AND budget_id = $bid;
  ```
  Matches Valkey's `HINCRBY soft_breach_count 1` at
  `flowfabric.lua:6614`.

**Isolation discipline + lock-mode correction (reviewer finding,
high-severity).** The existing `report_usage_impl` sets
`READ COMMITTED` at `budget.rs:168-171` per 0002's Q11 note
("budget ops run at READ COMMITTED + row-level `FOR UPDATE`, not
SERIALIZABLE"). Breach-counter increments stay on the same tx +
same isolation. **Lock-mode correction:** Revision 5's
`report_usage_impl` currently loads the policy row with
`FOR SHARE` at `budget.rs:226-234`. A pre-draft of Revision 6
claimed the breach UPDATE would "upgrade SHARE to EXCLUSIVE
implicitly" and serialize on the UPDATE. **That is wrong.** In
Postgres, if two concurrent txs both hold `FOR SHARE` on the same
row and both then execute `UPDATE` (which needs `FOR NO KEY
UPDATE`), each tx's UPDATE conflicts with the *other* tx's SHARE
lock → classic deadlock, not serialization. PG's deadlock detector
aborts one after `deadlock_timeout` (default 1s), surfacing a
`40P01` error the RC path does not retry. (Reviewer finding:
gemini-code-assist + cursor-bugbot on PR #350, Revision 6
Round-6.)

Revision 6 fixes this by **changing the policy-load lock-mode from
`FOR SHARE` to `FOR NO KEY UPDATE`** on the Standalone-1 impl of
`report_usage_impl`:

```sql
SELECT policy_json, breach_count, soft_breach_count
FROM ff_budget_policy
WHERE partition_key = $1 AND budget_id = $2
FOR NO KEY UPDATE;
```

`FOR NO KEY UPDATE` is the correct mode: it blocks concurrent
`FOR NO KEY UPDATE` / `FOR UPDATE` (serializing the breach path),
but does not block `FOR KEY SHARE` readers needed only for
referential-integrity locks. Two concurrent `report_usage_impl`
callers that both hit hard-breach now serialize on the initial
SELECT (not on the UPDATE), the correct cooperative-lock protocol.
No deadlock surface.

Contention cost vs. `FOR SHARE`: `FOR NO KEY UPDATE` strictly
serializes the policy-load (vs. shared concurrent reads under
`FOR SHARE`). For non-breach paths (the common case), policy-load
contention is single-row + partition-local; concurrent
`report_usage` calls for the same budget_id already serialize on
the per-dim `FOR UPDATE` at `budget.rs:273-283`, which is the real
admission-rate bottleneck. The lock-mode change adds zero new
serialization surface for non-breach paths in practice; for breach
paths it trades deadlock-risk for deterministic serialization —
the correct trade.

`breach_count` stays monotonic under RC + `FOR NO KEY UPDATE` (no
lost updates — the lock covers the read-modify-write sequence).
The SERIALIZABLE retry budget from §4.2.3 does **not** apply here:
§4.2.3 retries are for cancel_flow ack races with backlog
predicate reads, structurally absent from `report_usage_impl`.

**Hot-path latency impact (potential concern, flagged for
measurement).** Every `report_usage_impl` call that results in a
breach now runs one extra UPDATE. Non-breach calls are unaffected.
Breach UPDATEs are partition-local (same partition as the policy
row) + hit the primary-key partition child directly; cost is one
row UPDATE under a lock already held. Expected impact: <50μs
median overhead per breach; negligible on non-breach paths. If
benchmarks surface a regression post-impl, the alternatives are
(a) move breach counters to a separate `ff_budget_counters` table
(Gap 3 Option B, rejected for Wave 9) to avoid policy-row
contention, or (b) batch breach-counter updates via a dedicated
scanner. Both are follow-up moves, not Wave-9 blockers.

**`report_usage_admin` entry point.** Factor the shared body into
`report_usage_and_check_core(tx, pool, partition_config, budget,
dimensions, worker_instance: Option<&WorkerInstanceId>)`;
`report_usage_impl` calls with `worker_instance = Some(&w)`,
`report_usage_admin` calls with `worker_instance = None`. The
worker-instance argument is not used by the breach-counter
UPDATEs themselves (they are budget-scoped, not worker-scoped);
the split is only to avoid leaking a phantom `WorkerInstanceId`
field on the admin surface.

#### 4.4.7 `get_budget_status` — 2-SELECT shape (Gap 4)

Revision 5's "3 SELECTs (definition + usage + limits)" assumed
limits lived in a separate Hash (mirroring Valkey's
`budget_limits` key). In the Revision 6 Postgres design,
`hard_limits` + `soft_limits` are persisted inside
`ff_budget_policy.policy_json` by the Standalone-1 `create_budget`
impl (§4.4.2) — not a separate limits table. (The shape is
already prefigured by the existing `upsert_policy_for_test`
helper at `budget.rs:374-400`, marked `#[doc(hidden)]`, which
writes `policy_json` via `INSERT ... ON CONFLICT DO UPDATE`;
`create_budget` ships the same shape with the additional
columns added by migration 0013.) Revision 6 collapses to 2
SELECTs:

```sql
-- SELECT 1: policy row (definitional + counters + scheduler).
SELECT scope_type, scope_id, enforcement_mode,
       breach_count, soft_breach_count,
       last_breach_at_ms, last_breach_dim,
       next_reset_at_ms, created_at_ms,
       policy_json
FROM ff_budget_policy
WHERE partition_key = $1 AND budget_id = $2;

-- SELECT 2: usage rows (one per dimension).
SELECT dimensions_key, current_value
FROM ff_budget_usage
WHERE partition_key = $1 AND budget_id = $2;
```

Empty SELECT 1 → `EngineError::NotFound { entity: "budget" }` with
a `backend_context` wrapper carrying the budget_id (matches
Valkey's behavior at `lib.rs:5940-5951`). `hard_limits` +
`soft_limits` are parsed from `policy_json` via the existing
`limits_from_policy` helper (`budget.rs:243-244`). `usage` is the
`dimensions_key → current_value` map from SELECT 2. The 9
`BudgetStatus` `*_at_ms` integer columns are formatted to `Option<String>`
via the same non-empty wrapper Valkey uses (`lib.rs:5985-5987`) so
the contract shape matches byte-for-byte across backends.

No JOIN — the two SELECTs are executed sequentially on the same
connection (not in a transaction; budget status is a snapshot read
and the two queries racing a concurrent `reset_budget` is
acceptable — same eventual-consistency semantic as Valkey's 3×
HGETALL). A single-query `LEFT JOIN` form was considered and
rejected: per-dimension fan-out complicates deserialization + the
two-query form stays partition-local on both reads.

#### 4.4.8 Summary of Standalone-1 query shapes (post-Revision-6)

| Method | Query shape | Table(s) touched |
|---|---|---|
| `create_budget` | 1 INSERT ON CONFLICT | `ff_budget_policy` |
| `reset_budget` | 2 UPDATEs + RETURNING (SERIALIZABLE tx) | `ff_budget_usage`, `ff_budget_policy` |
| `create_quota_policy` | 1 INSERT ON CONFLICT | `ff_quota_policy` |
| `get_budget_status` | 2 SELECTs | `ff_budget_policy`, `ff_budget_usage` |
| `report_usage_admin` | shared `report_usage_and_check_core` (RC tx) | `ff_budget_policy`, `ff_budget_usage`, `ff_budget_usage_dedup` |

### 4.5 Standalone-2 — `list_pending_waitpoints`

Real table is `ff_waitpoint_pending`
(`0001_initial.sql:298`), **not** `ff_waitpoint`. It is HASH-
partitioned 256 ways with PK `(partition_key, waitpoint_id)` and
has **no `status` column** — presence in the table implies pending
(signal-delivery moves rows out).

Columns actually present today:
`partition_key, waitpoint_id, execution_id, token_kid, token,
created_at_ms, expires_at_ms, condition`.

**Contract ground truth (A3).** The trait return type is
`PendingWaitpointInfo` (`crates/ff-core/src/contracts/mod.rs:822-859`),
not the invented 5-field struct in Revision 2. The real contract has
10 fields:

```
waitpoint_id, waitpoint_key, state, required_signal_names,
created_at, activated_at (Option), expires_at (Option),
execution_id, token_kid, token_fingerprint
```

Phantom fields `flow_id` and `attempt_index` are NOT in the
contract and are dropped from the projection.

**Data-availability gap + migration 0011.** Of the 10 contract
fields, three have no source column in the existing schema:
`state`, `required_signal_names`, `activated_at`. (A fourth field,
`waitpoint_key`, was mis-scoped as missing in Revision 3/4; it
already exists on `ff_waitpoint_pending` as `TEXT NOT NULL DEFAULT ''`
since migration 0004 and is populated by `suspend_ops.rs` on insert.
Corrected in Revision 5.) None of the three missing fields is
derivable cleanly:

- `state` — `'pending' | 'active' | 'closed'`. Revision 2
  claimed "presence in the table implies pending"; this collapses
  `pending` and `active` (activation is a separate state in the
  contract). Cannot be derived without a column.
- `required_signal_names` — derivable in principle from
  `condition jsonb`, but the jsonb shape is opaque producer-side;
  extracting names requires the backend to understand every
  condition variant. Fragile.
- `activated_at` — the timestamp when the waitpoint transitioned
  `pending → active`. No historical source.

Revision 3 adds migration 0011 with additive columns (Revision 5:
3 columns, not 4):

```sql
ALTER TABLE ff_waitpoint_pending
    ADD COLUMN state                 TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN required_signal_names TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN activated_at_ms       BIGINT;
```

All columns are nullable-or-defaulted → additive and
`backward_compatible = true`. Existing rows inserted pre-0011 get
`state = 'pending'`, empty `required_signal_names`, NULL
`activated_at_ms`. New inserts (post-0011) populate
`required_signal_names` from the producer-side `suspend_*` path;
activation transitions populate `state = 'active', activated_at_ms = now`.
The producer + activation wiring changes land in the same PR as
migration 0011 (§6.2 step 4) — without them the table has usable
defaults but degraded fidelity on already-inserted rows.

**Field mapping (post-0011):**

- `waitpoint_id`, `waitpoint_key`, `execution_id`, `created_at`
  (from `created_at_ms`), `expires_at` (from `expires_at_ms`),
  `token_kid` — direct columns (`waitpoint_key` from migration
  0004).
- `token_fingerprint` — computed from `token` at projection
  (redaction per Stage D1 contract; never returned raw).
- `state`, `required_signal_names`, `activated_at` (from
  `activated_at_ms`) — direct columns post-0011.

No JOIN to `ff_exec_core` / `ff_attempt` required — the phantom
`flow_id`/`attempt_index` fields are not in the contract.

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

Query sketch (post-0011):

```sql
SELECT wp.partition_key,
       wp.waitpoint_id,
       wp.execution_id,
       wp.token_kid,
       wp.token,           -- fingerprinted in projection, never returned raw
       wp.waitpoint_key,
       wp.state,
       wp.required_signal_names,
       wp.created_at_ms,
       wp.activated_at_ms,
       wp.expires_at_ms
FROM ff_waitpoint_pending wp
WHERE (wp.partition_key, wp.waitpoint_id) > ($after_pk, $after_wp)
ORDER BY wp.partition_key, wp.waitpoint_id
LIMIT $limit;
```

Single-table scan; no JOIN.

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
   cross-coupling with spine-A/B. Lands **migrations 0012 + 0013**
   (§4.4.5): 0012 adds the `ff_quota_policy` family (3 tables);
   0013 extends `ff_budget_policy` with 8 additive columns +
   partial index on `next_reset_at_ms`. Lands the new
   `BudgetResetReconciler` at
   `crates/ff-backend-postgres/src/reconcilers/budget_reset.rs`
   (§4.4.3) + wires it into `reconcilers/mod.rs::register_all`
   so `examples/token-budget` sees periodic resets on Postgres
   matching Valkey. Extends `report_usage_impl` with breach-counter
   UPDATEs (§4.4.6) — Revision 5's §7.2 pin lifted narrowly for
   this extension only. Unblocks `examples/token-budget` on
   Postgres (§6.3 release-gate).
3. **Spine-A pt.1 — `cancel_execution` + `revoke_lease` +
   `change_priority` (3 methods).** Tables:
   `ff_exec_core` / `ff_attempt` / `ff_lease_event` (existing);
   `ff_operator_event` (**new, migration 0010**). Proves the
   SERIALIZABLE-fn + outbox template and lands the operator-event
   channel that spine-A pt.2 also uses.
4. **Standalone-2 — `list_pending_waitpoints` (1 method).**
   Lands **migration 0011** (additive columns on
   `ff_waitpoint_pending`) + producer-side writes that populate the
   new columns on `suspend_*` / activation transitions.
   Partition-cursor shakedown.
5. **Spine-A pt.2 — `cancel_flow_header` + `ack_cancel_member` +
   `replay_execution` (3 methods).** Introduces
   `ff_cancel_backlog` + `ff_cancel_backlog_member` (migration
   0014, after Standalone-1's 0012+0013); emits on the
   `ff_operator_event` channel landed in step 3.

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
  columns. Wave 9 adds:
  - `ff_operator_event` outbox table + trigger (migration 0010,
    lands with spine-A pt.1).
  - Additive columns on `ff_waitpoint_pending` (migration 0011,
    lands with standalone-2 step).
  - `ff_quota_policy` + `ff_quota_window` + `ff_quota_admitted`
    (migration 0012, lands with Standalone-1 step).
  - Additive columns + partial index on `ff_budget_policy`
    (migration 0013, lands with Standalone-1 step).
  - `ff_cancel_backlog` + `ff_cancel_backlog_member` (spine-A pt.2
    step, numbered 0014 after 0013 in impl-order).
  No destructive DDL.
- **No stop-the-world.** `CREATE INDEX CONCURRENTLY` for any
  indexes added post-0001. Table creation + annotation inserts
  are cheap + locking-bounded.
- **backward_compatible = true** in every `ff_migration_annotation`
  row. Operators can roll backward to any pre-Wave-9 ff-server
  binary; extra tables sit idle.
- **Rollback is forward-only-stateful (B-2).** Post-0011 binary
  populates `state = 'active'` + `activated_at_ms` on the
  pending→active waitpoint activation transition. A rolled-back
  pre-0011 binary does not know about these columns and will not
  mutate `state` back to `'pending'` on the reverse transition;
  already-activated rows persist their post-0011 `state` value
  until post-0011 code processes them again. Full rollback-safety
  (i.e. a rolled-back binary silently ignoring Wave-9-written
  columns and staying semantically consistent) is out-of-scope;
  `backward_compatible = true` here means the schema is additive
  and old binaries boot, not that Wave-9-written state is
  round-trippable through a rollback.

### 6.3 Release + capability-flip protocol

**Capability-flip timing — atomic at the release PR (B3/C3d).**
Revision 2 offered either per-step-flip or release-PR-flip as
"both acceptable." Revision 3 collapses to a single rule, matching
§2.2's "single atomic capability-surface flip at release time":

- Steps 1–5 land method impls + migrations behind `Supports` flags
  that stay `false`. The trait impl is correct on-disk; the flag
  continues to report `false` so capability-discovery consumers
  see no churn during the wave's landing window.
- The **final release PR** (no new method impls) performs a single
  atomic flip:
  - Flips all 13 `Supports` flags in §3.1 from `false` → `true`.
  - Updates `crates/ff-backend-postgres/tests/capabilities.rs:53-73`
    to assert `true` for the 13 flags.
  - Updates `docs/POSTGRES_PARITY_MATRIX.md` to drop the `stub`
    column for every Wave-9 method.
  - **Line item (B4):** flips the stale
    `rotate_waitpoint_hmac_secret_all` matrix row to `impl`
    (housekeeping, §3.3 — already shipped in-tree but docs lag).
  - **Line item (B1):** commits `docs/CONSUMER_MIGRATION_0.11.md`
    (matches v0.8 / v0.10 precedent); documents capability-flag
    flips, new outbox channel (`ff_operator_event`), new waitpoint
    columns.
  - Appends v0.11 `CHANGELOG.md` section.

**Release gate additions (B2).** In addition to the CLAUDE.md §5
gate (smoke before tag, examples build, parity matrix current,
README sweep, etc.), Wave 9 adds:

- **`examples/token-budget/` runs green on `FF_BACKEND=postgres`.**
  This example exercises Standalone-1 (create_budget / reset_budget
  / create_quota_policy / get_budget_status / report_usage_admin)
  end-to-end — including `status.breach_count` / `soft_breach_count`
  post-mortem reads at `examples/token-budget/src/main.rs:200-201`,
  which require the migration 0013 columns + the §4.4.6
  `report_usage_impl` breach-counter extension. Revision 6 keeps
  §6.3's existing gate unchanged; the Standalone-1 PR (§6.1 step 2)
  is the milestone at which this gate starts passing. It must PASS
  on the Postgres backend before tag; a failure withdraws the
  release (whole-wave-or-withdraw).
- Smoke runs against the published artifact per
  [feedback_smoke_before_release].

---

## 7. Open questions (resolved / to-owner)

Revision 2 resolves most Round-1 forks in-RFC (no new forks added).
Remaining items are flagged explicitly.

### 7.1 Read-model shape — **RESOLVED in §4.1**

Outcome: join-on-read against real schema, no `ff_execution_view`
projection. Cairn's call pattern is one-exec-at-a-time on operator
click; projection-table write amplification across every state
transition is not justified. See §4.1.

### 7.2 Budget counter granularity — **RESOLVED in §4.4 (Revision 6 refinement)**

`report_usage_impl` already ships
(`crates/ff-backend-postgres/src/budget.rs:154`). The per-dimension
granularity choice is already made in-tree; admin methods commit to
match that shape. Revision 6 narrowly extends `report_usage_impl`
(§4.4.6) to maintain `breach_count` / `soft_breach_count` /
`last_breach_at_ms` / `last_breach_dim` on `ff_budget_policy`
(migration 0013) — this is the Gap-3 resolution. The pin from
Revisions 2-5 ("don't touch the shipped hot-path code") is lifted
for this specific additive UPDATE and kept in place for all other
aspects of `report_usage_impl` (isolation level, FOR UPDATE
per-dim row locking, dedup shape). See §4.4.6 for isolation +
contention analysis; potential-hot-path-impact flagged but
expected to be <50μs per breach on partition-local UPDATEs.

### 7.3 Operator-control outbox emission — **RESOLVED in §4.2.7 (Option X in Revision 3)**

Yes-emit for every mutating Spine-A method, routed to
`ff_lease_event` for lease-affecting ops and the **new
`ff_operator_event` channel** (migration 0010, §4.3.1) for
priority/replay/flow-cancel events. Matrix in §4.2.7.

Revision 2 routed these onto `ff_signal_event`; Round-2 found that
was a schema-impossible + subscriber-contract-breaking choice
(A1/A2). Revision 3 introduces the dedicated channel.

### 7.4 Reconciler interaction + cancel/replay race — **RESOLVED in §4.2.6**

`SELECT ... FOR UPDATE` + SERIALIZABLE isolation + CAS on fencing
columns give reconciler-symmetric atomicity. Detail in §4.2.6.

### 7.5 Cancel-backlog shape — **RESOLVED in §4.3**

Junction table (`ff_cancel_backlog_member`) over array-column
(`BYTEA[]`), both 256-way partitioned. Junction composes better
with partitioned `FOR UPDATE` row-level locking.

### 7.6 Capability-flip timing — **RESOLVED in §6.3 (atomic-at-release-PR in Revision 3)**

Owner directive is coherent wave in one release. Revision 3 pins
the flip to the release PR only (single atomic flip); per-step-flip
variant is withdrawn to align with §2.2.

### 7.7 — (withdrawn; `rotate_waitpoint_hmac_secret_all` housekeeping folded into §6.3)

### 7.8 `get_execution_result` attempt indexing — **RESOLVED in §4.1**

Matches Valkey's `GET ctx.result()`
(`crates/ff-backend-valkey/src/lib.rs:5254-5273`) which reads the
current attempt's result. `ff_exec_core.result` is the binding
column; no `attempt_index` input. Per-attempt-history is out of
scope for Wave 9.

**C3c forward-commitment note.** Picking current-attempt semantics
for `get_execution_result` means a per-attempt-history API cannot
be added by extending this method's signature later (trait methods
are non-breaking-only in consumer contract); a future
"per-attempt-history" surface would need a new method
(`get_execution_result_history` or similar). This is acceptable:
Valkey's primitive is also current-attempt, and per-attempt history
is not in any known consumer ask. If the need arises, it's a
new-method addition — not an API break.

### 7.9 `ack_cancel_member` silent-on-both parity — **RESOLVED (C3b)**

Neither backend emits an outbox row on `ack_cancel_member` (Valkey
does not XADD per-member; §4.2.7 matches). This is the intended
forward shape: per-member outbox traffic would be proportional to
flow size, and the header-level `flow_cancel_requested` event
already drives all observer re-projection needs. No owner-call
fork.

### 7.11 Budget-policy row contention on breach-UPDATE (Revision 6, flagged)

Revision 6 §4.4.6 extends `report_usage_impl` to UPDATE
`ff_budget_policy.breach_count` on every hard-breach call. Under
concurrent hard-breach load (same budget_id, many simultaneous
`report_usage` callers), these UPDATEs serialize on the policy
row's EXCLUSIVE lock. Expected scenarios see <10 concurrent
breaches/budget/sec (breaches are exceptional — typical load
returns OK/SoftBreach), so this is not expected to be a hotspot.
If post-impl benchmarks surface >500μs p99 on hard-breach paths,
the fallback is to migrate breach counters to a separate
`ff_budget_counters` table (Gap 3 Option B). This is an
additive migration — the policy-row counters become derived
history, and the hot-path writes move to the separate table with
a sharded-counter shape. Tracked as a Wave-10 follow-up; not a
Wave-9 blocker. Flagged per Revision 6's Gap-3 analysis.

### 7.12 Quota-policy concurrency-column contention (Revision 6, flagged)

Migration 0012 lands `active_concurrency` as a column on
`ff_quota_policy`. Admission + release UPDATEs `SET
active_concurrency = active_concurrency +/- 1` under a `FOR UPDATE`
row lock, serializing all admission RPS against the single row.
Wave 9 ships the quota *schema* + `create_quota_policy` only —
admission (`ff_check_admission_and_record` parity) is not
Wave-9 scope. At typical admission RPS (hundreds/sec/policy)
the single-row lock is fine; at RPS >10k/policy/sec the row
becomes a hotspot. Mitigation: split to a separate
`ff_quota_concurrency` table with sharded-counter shape when the
hot-path admission impl lands. Flagged here so the admission
RFC (future, not Wave 9) addresses it before shipping hot-path
code against a contention-prone shape.

### 7.13 Remaining owner-call items (push if needed)

None. Revision 3 resolved Round-2 findings (Option X outbox,
additive waitpoint migration, atomic-at-release-PR flip, C3a–d).
Revision 6 adds two flagged-but-not-blocking open questions (§7.11
+ §7.12) around post-impl contention measurement — both have
additive follow-up migrations as the mitigation path, neither is
a Wave-9 blocker. Revision 7 resolves three impl-surfaced forks
(§7.14/§7.15/§7.16 below) — none are owner-call items, all three
resolved under §5.2 mandate without scope change. If the owner
disagrees with any resolution (Option X vs Y; additive-migration
vs join-derive for waitpoint; `get_execution_result` semantics;
ack silence; §7.11 / §7.12 hotspot tolerance; §7.14 / §7.15 /
§7.16 translations), §7 reopens the specific item.

### 7.14 `replay_execution` skipped-flow-member path on Postgres — **RESOLVED in §4.2.5 (Revision 7, Fork 1 Option A)**

Valkey's skipped-flow-member replay (`flowfabric.lua:8557-8615`)
resets per-`dep_edge` state + recomputes `deps_meta` counters.
Postgres has no `ff_dep_edge` / `ff_deps_meta` — dep gating lives
on `ff_edge_group` counters. Spine-A pt.2 impl attempt surfaced
that Revision 6's "reset upstream-edge states matching Valkey"
had no column to write against.

Outcome: Option A — reset downstream's `ff_edge_group` counters
to neutral (`running_count = 0`, `skip_count = 0`, `fail_count = 0`;
`success_count` preserved, mirroring Valkey's "satisfied edges
remain satisfied" at `flowfabric.lua:8580`). Concrete SQL in
§4.2.5. Feasibility confirmed against `0001_initial.sql:217-230`
(no CHECK constraints on counter columns). Double-bump concern
self-resolves via `running.max(0)` clamp in
`dispatch.rs:336`.

Rejected alternatives documented in §8.8: Option B (delete +
recreate `ff_edge_group`), Option C (mark runnable without
counter reset), Option D (declare not-supported / return
`EngineError::Unavailable`).

### 7.15 `attempt_index` semantics on normal-replay — **RESOLVED in §4.2.5 (Revision 7, Fork 2 Option A)**

Revisions 4-6 §4.2.5 specified `attempt_index + 1` + INSERT new
`ff_attempt` row on normal replay. Valkey (`flowfabric.lua:8625-8650`)
mutates in place without bumping `attempt_index` (no
`current_attempt_index + 1`, no new hash). Spine-A pt.2 impl
noted the divergence would leak through `get_execution_info().
current_attempt_index` across backends.

Outcome: Option A — in-place UPDATE on the existing `ff_attempt`
row, no `attempt_index` bump, matching Valkey exactly. §5.2
locks "no Valkey behavior change"; the historical-audit
nice-to-have that Revision 4's INSERT shape introduced was not
Valkey-canonical and is dropped. §4.2.6 A4 scanner-hazard
audit updated accordingly.

Rejected alternatives documented in §8.9: Option B (keep RFC
divergence, document as cross-backend observable difference),
Option C (amend `ff_attempt` schema with
`replayed_from_attempt_index` audit column to preserve in-place
semantic with history).

### 7.16 `change_priority` return shape on already-terminal — **RESOLVED in §4.2.4 (Revision 7, Fork 3 Option C)**

Revision 6 §4.2.4 specified row-count=0 → return `AlreadyTerminal`.
`ChangePriorityResult` contract (`crates/ff-core/src/contracts/mod.rs:445-448`)
has only a `Changed` variant — the specified variant did not exist.

Outcome: Option C → mirror Valkey. Valkey's `ff_change_priority`
(`flowfabric.lua:3683-3688`) returns
`err("execution_not_eligible")` on any non-runnable / non-
eligible_now state; this surfaces as an `EngineError` in the
Rust layer (`crates/ff-script/src/error.rs:218,663`), **not** as
an outcome-enum variant. Postgres matches: row-count=0 maps to
the same `EngineError` mapping. `ChangePriorityResult` stays
single-variant; no contract amendment needed for Wave 9.

Rejected alternatives documented in §8.10: Option A (amend
`ChangePriorityResult` to add `AlreadyTerminal` variant — a
pre-1.0 breaking contract change, not Valkey-canonical), Option
B (return `EngineError::Validation` instead of mirror of
`execution_not_eligible` — would introduce a different error
shape than Valkey emits).

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

**Rejected.** The schema decisions in Revision 6 remain contained:
join-on-read against existing columns (no new projection tables);
one new backlog pair; one new operator-event outbox channel
(migration 0010); three additive columns on an existing table
(migration 0011); the `ff_quota_policy` family (migration 0012)
mirroring Valkey's already-shipped quota key layout one-to-one;
and additive columns on `ff_budget_policy` (migration 0013) that
close `BudgetStatus` data-availability gaps. None of these is a
forward-facing primitive that warrants a separate RFC audience —
they are all mechanical prerequisites for the method impls this
RFC proposes, each traceable to a specific Valkey-canonical shape.
If a future Wave-10 introduces projection tables or cross-cutting
schema shapes, it is welcome to split at that time. RFC-020 stays
single-doc.

### 8.6a Repurpose `ff_signal_event` for operator-control events

Rejected in Rev-3 (§4.2.4 / §4.2.7): schema mismatch (no
`event_type` column, `signal_id NOT NULL`) + subscriber-contract
fork against the existing "signal-delivery" channel semantic. See
§4.2.4 channel-choice rationale; migration 0010 introduces the
dedicated `ff_operator_event` channel instead.

### 8.7 Split Wave 9 across v0.11 + v0.12

Considered: land Spine-B + Standalone-1 in v0.11; Spine-A +
Standalone-2 in v0.12. Argument: smaller per-release risk surface;
reviewer-C's "ship-G4+G5-only" variant.

**Rejected (owner call).** Coherent ship is the full-parity
promise; splitting means cairn pins across two minors on an
incomplete capability set. Whole-wave-or-withdraw: the split-RFC
shape is explicitly the failure mode the directive is avoiding.

**Per-release-risk engagement (C1).** Reviewer C's argument on the
merits: splitting Wave 9 halves the DDL + method surface changing
per release, shrinking the blast radius of any regression
discovered post-tag. If Spine-B + Standalone-1 land in v0.11 and a
bug surfaces, v0.11.1 is a narrow hotfix over 8 methods, not 13;
the v0.12 spine-A ship can absorb the fix and continue. This is
the standard "smaller deployable unit" argument and it is not
frivolous.

The counter-argument, specific to Wave 9's shape: the design
**spine** (§4.2) is one template applied 6 times; the risk delta
between shipping 3 spine methods in v0.11 + 3 in v0.12 and
shipping all 6 in v0.11 is smaller than method count suggests,
because the spine template is the unit of review and the unit of
bug — a spine regression is 6-method-wide regardless of the ship
cadence. Meanwhile, the cost of splitting is structural: cairn
pins across two minors rather than one, the capability-discovery
surface flips twice (operator-UI greys-out state changes twice),
the CHANGELOG + CONSUMER_MIGRATION doc is duplicated, and the
"Postgres != parity" state persists for an extra release cycle
against the full-parity directive. On net, the coherence-per-
release cost of splitting exceeds the per-release-risk benefit
given the spine's template-shaped risk surface. Single-release is
the better trade for this specific wave; per-release-risk would
dominate if the 13 methods were 13 independent templates, but they
are not.

### 8.8 `replay_execution` skipped-flow-member — Options B / C / D (rejected per Revision 7 Fork 1)

Fork 1 considered four translations of Valkey's
`ff_dep_edge` + `ff_deps_meta` reset onto Postgres's
`ff_edge_group` counter shape; Revision 7 picks Option A
(§4.2.5 + §7.14). The three rejected options:

- **Option B — delete + recreate `ff_edge_group` rows.** Clean-
  slate reset: `DELETE FROM ff_edge_group WHERE downstream_eid =
  $id` then re-`INSERT` from an edge-policy join. Rejected
  because it loses `policy` / `k_target` / `policy_version` on
  every replay, forcing a JOIN to `ff_edge` + re-derivation of
  the policy payload — adds a read hop to the replay hot path
  for no observable gain over Option A's in-place counter reset.
- **Option C — mark `exec_core.lifecycle_phase = 'runnable'` but
  leave `ff_edge_group` counters untouched.** Rejected as
  behaviorally broken: `skip_count > 0` continues to satisfy the
  dispatch-path evaluator's terminal-signal test, so the
  replayed exec stays in `'runnable'` + dependency-gated forever
  — eligible-but-never-promoted. Replay UI shows "replayed" but
  the exec never re-runs; a silent failure mode.
- **Option D — return `EngineError::Unavailable` for
  skipped-flow-member replay on Postgres.** Honest-divergence
  path: normal replay works fully, skipped-flow-member replay
  documented as Postgres-only-not-supported. Rejected because
  §5.2 locks full parity — cairn's replay UI would partially
  grey-out on Postgres even though Option A is feasible against
  the shipped `ff_edge_group` schema. Option D remains the
  fallback if Option A's counter reset surfaces a hidden
  constraint violation during Spine-A pt.2 impl; if that
  happens, reopen §7.14.

### 8.9 `replay_execution` normal path — Options B / C (rejected per Revision 7 Fork 2)

Fork 2 considered three translations of Valkey's in-place
replay; Revision 7 picks Option A (§4.2.5 + §7.15). The two
rejected options:

- **Option B — keep Revision 4-6's `attempt_index + 1` +
  INSERT-new-row divergence; document explicitly as a
  cross-backend observable difference.** Rejected: §5.2 locks
  "no Valkey behavior change"; consumers observing
  `get_execution_info().current_attempt_index` would see
  different index values across backends on the same logical
  event, which is a parity break. The historical-audit
  nice-to-have that motivated Revision 4's bump was not
  Valkey-canonical in the first place.
- **Option C — amend `ff_attempt` schema with
  `replayed_from_attempt_index` audit column; keep in-place
  UPDATE semantic while preserving history via the audit
  column.** Rejected: additive schema change for a non-
  Valkey-canonical feature. If a per-attempt-history surface is
  needed later, it is a separate RFC (matching the §7.8 C3c
  forward-commitment shape for `get_execution_result`) — not
  something to bolt into Wave 9 Spine-A.

### 8.10 `change_priority` already-terminal return shape — Options A / B (rejected per Revision 7 Fork 3)

Fork 3 considered three shapes for row-count=0 on the UPDATE;
Revision 7 picks Option C → mirror Valkey (§4.2.4 + §7.16). The
two rejected options:

- **Option A — amend `ChangePriorityResult` contract with an
  `AlreadyTerminal` variant.** Would require a breaking contract
  change (pre-1.0, so nominally in-window, but Wave 9's §5.2
  locks "no Valkey behavior change" — and Valkey does not return
  an outcome-enum variant here, it returns an error). Adding a
  variant Valkey does not emit is inventing a new parity shape
  just for Postgres, which inverts the spine's binding semantic.
- **Option B — return `EngineError::Validation { kind:
  InvalidInput, message: "execution already terminal" }`.**
  Rejected: Valkey returns a specific
  `execution_not_eligible` error (not a generic validation
  error); consumers pattern-matching that specific error code
  would miss on Postgres if the mapping differs. Option C
  mirrors the Valkey code exactly — no consumer contract
  divergence.

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
  — schema source for A1 (no `event_type` column; `signal_id NOT
  NULL`) driving the Option X decision
- `crates/ff-backend-postgres/migrations/0008_lease_event_instance_tag.sql`
  / `0009_signal_event_instance_tag.sql` — RFC-019 instance-tag
  columns; `ff_operator_event` (§4.3.1) ships with parity columns
  in its initial DDL
- `crates/ff-core/src/contracts/mod.rs:822-859` —
  `PendingWaitpointInfo` contract ground truth (A3)
- `crates/ff-backend-postgres/src/lib.rs` — current `Unavailable`
  sites (grep `EngineError::Unavailable`)
- `crates/ff-backend-postgres/src/budget.rs:154` — shipped
  `report_usage_impl` (the §4.4 / §7.2 binding shape; Revision 6
  §4.4.6 amends with breach-counter UPDATEs)
- `crates/ff-backend-postgres/migrations/0002_budget.sql` — real
  budget schema (Revision 6 Gap-3 base: `ff_budget_policy` +
  `ff_budget_usage` + `ff_budget_usage_dedup`; 0013 extends
  `ff_budget_policy` with 8 columns)
- `crates/ff-core/src/contracts/mod.rs:1388-1407` —
  `CreateQuotaPolicyArgs` / `CreateQuotaPolicyResult` (Revision 6
  Gap-1 contract)
- `crates/ff-core/src/contracts/mod.rs:1413-1430` — `BudgetStatus`
  contract ground-truth (Revision 6 Gap-3 driver: 9 fields without
  column sources pre-0013)
- `crates/ff-core/src/keys.rs:573-615` — `QuotaKeyContext` Valkey
  key layout (Revision 6 Gap-1 shape reference for 0012)
- `crates/ff-script/src/flowfabric.lua:6451-6676` — Valkey
  `ff_create_budget` / `ff_report_usage_and_check` / `ff_reset_budget`
  (Revision 6 §4.4.6 breach-counter pattern source)
- `crates/ff-script/src/flowfabric.lua:6839-6878` — Valkey
  `ff_create_quota_policy` (Revision 6 Gap-1 reference)
- `crates/ff-backend-valkey/src/lib.rs:5109-5204` + `5921-6010`
  — Valkey Rust impls of Standalone-1's 5 admin methods
- `crates/ff-engine/src/scanner/budget_reset.rs` — Valkey budget-
  reset scanner shape (Revision 6 §4.4.3 basis for the Postgres-
  native `reconcilers/budget_reset.rs`)
- `crates/ff-backend-postgres/src/reconcilers/lease_expiry.rs` —
  the §4.2.6 race partner
- `crates/ff-backend-postgres/src/reconcilers/attempt_timeout.rs`
  — per-attempt scope audit (A4, §4.2.6)
- `crates/ff-backend-postgres/src/dispatch.rs:300-339` — Revision 7
  Fork 1 reference: `ff_edge_group` counter-update template
  (`running -= 1`, outcome-bucket increment, invariant
  `total = success + fail + skip + running`) that the
  §4.2.5 skipped-flow-member reset mirrors in reverse
- `crates/ff-backend-postgres/src/edge_cancel_dispatcher.rs:234` —
  Revision 7 Fork 1 reference: second site maintaining
  `ff_edge_group` counters on terminal transitions; invariant
  audit confirmed reset-to-neutral-then-rebuild is symmetric to
  the existing write path
- `crates/ff-script/src/flowfabric.lua:3683-3688` — Revision 7
  Fork 3 reference: Valkey `ff_change_priority` returns
  `err("execution_not_eligible")` on non-runnable / non-eligible
  state (the mirror-target for §4.2.4's row-count=0 mapping)
- `crates/ff-script/src/error.rs:218,663` — Revision 7 Fork 3
  reference: Rust error mapping for `execution_not_eligible`
  surfaced via `EngineError`
- `crates/ff-core/src/contracts/mod.rs:445-448` — Revision 7
  Fork 3 reference: `ChangePriorityResult` contract; stays
  single-variant under Revision 7 resolution
- `crates/ff-script/src/flowfabric.lua:8557-8650` — Revision 7
  Fork 1 + Fork 2 reference: Valkey `ff_replay_execution`
  canonical semantics, both skipped-flow-member path
  (8557-8615) and normal-replay path (8619-8650)
- `crates/ff-backend-valkey/src/lib.rs:5254-5273` —
  `get_execution_result` current-attempt semantics (§4.1, §7.8)
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
