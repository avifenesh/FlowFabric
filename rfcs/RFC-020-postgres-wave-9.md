# RFC-020: Postgres Wave 9 — deferred backend impls

**Status:** ACCEPTED — Revision 4
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Accepted:** 2026-04-26
**Revision 2:** 2026-04-26 — Round-1 reviewer findings addressed (schema-ground-truth, design-spine reframe, drop G6, full-parity + coherent-wave directive)
**Revision 3:** 2026-04-26 — Round-2 reviewer findings addressed: (A1/A2) new `ff_operator_event` channel (migration 0010) instead of repurposing `ff_signal_event`; (A3) `PendingWaitpointInfo` contract aligned, additive migration 0011 adds `waitpoint_key`/`state`/`required_signal_names`/`activated_at_ms`; (A4) reconciler per-attempt scoping audited; (A5) `outcome`/`lifecycle_phase` column-binding clarified; (B1–B4) release-PR line items tightened; (C1) per-release-risk engagement in §8.7; (C2) intra-ack race traced to SERIALIZABLE retry; (C3a–d) resolved in §4.2.4/§4.2.7/§4.1/§6.3. Scope grows by two additive migrations (0010 + 0011); method count unchanged at 13.
**Revision 4:** 2026-04-26 — Round-3 reviewer-A findings addressed: (NEW-1) `lifecycle_phase` enum literals corrected to lowercase real-enum values (`cancelled` / `runnable` / `NOT IN ('terminal','cancelled')`) across §4.2.1, §4.2.4, §4.2.5, §4.2.6; (NEW-4) stray `FOR UPDATE` removed from UPDATE in §4.2.4; (NEW-5) `waitpoint_key` pre-0011 projection behavior pinned in §4.5 (COALESCE to `''` + degraded-row counter); (NEW-2) SERIALIZABLE retry budget pinned to 3 (matches `CANCEL_FLOW_MAX_ATTEMPTS` in `flow.rs:52`) in §4.2.3; (B-1) scope note for the producer-side `suspend_*` write-path change added to §3; (B-2) rollback-forward-only caveat added to §6.2; (C-polish) §8 adds a one-liner pointing at §4.2.4 for the repurpose-`ff_signal_event` rejection. No scope, alternatives, or new forks re-opened.
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

Revision 3 adds two additive migrations to the wave:

- **0010 `ff_operator_event` outbox** — new channel for operator-
  control events (`priority_changed` / `replayed` /
  `flow_cancel_requested`). Preserves the RFC-019 `ff_signal_event`
  subscriber contract (§4.2.7, §4.3). Option X per Round-2 §central
  decision.
- **0011 `ff_waitpoint_pending` additive columns** — `waitpoint_key`,
  `state`, `required_signal_names`, `activated_at_ms`. Required to
  serve the real `PendingWaitpointInfo` contract (§4.5).

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
side `suspend_*` write path to populate the 4 new 0011 columns
(`waitpoint_key`, `state`, `required_signal_names`, `activated_at_ms`)
on initial insert + activation transitions. This is in-scope
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
  transition after pre-read) → return `AlreadyTerminal`.
  (Revision 4: Revision 3's draft had a stray `FOR UPDATE` on the
  `UPDATE`, which is invalid SQL — `FOR UPDATE` is SELECT-only.
  The row-lock comes from the pre-read SELECT above per the §4.2
  template.)
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

- **Base path.** SERIALIZABLE tx:
  1. `SELECT ec.lifecycle_phase, ec.flow_id, ec.attempt_index,
     a.outcome FROM ff_exec_core ec LEFT JOIN ff_attempt a
     ON (a.partition_key, a.execution_id, a.attempt_index) =
     (ec.partition_key, ec.execution_id, ec.attempt_index)
     FOR UPDATE`.
     Column binding (A5): `lifecycle_phase` / `flow_id` /
     `attempt_index` live on `ff_exec_core`; `outcome` lives on
     `ff_attempt`. The join pins to the current-attempt row only.
  2. Require `lifecycle_phase = 'terminal'` (else `NotReplayable`).
  3. `UPDATE ff_exec_core SET lifecycle_phase = 'runnable',
     terminal_at_ms = NULL, result = NULL, attempt_index =
     attempt_index + 1, cancellation_reason = NULL, cancelled_by =
     NULL`. The post-replay phase is `'runnable'` (not `'pending'`)
     to match Valkey's `ff_replay_execution` body, which writes
     `"lifecycle_phase", "runnable"` on the base normal-replay path
     (`crates/ff-script/src/flowfabric.lua:8625`) and on the
     skipped-flow-member path (same file, line 8591). `'pending'`
     in the real enum denotes a freshly-created execution pre-
     dependency-resolution — a different state than post-replay
     reset. (Skipped-flow-member variant may additionally transition
     through blocked/eligibility sub-states via the upstream-edge
     reset; those are scheduler secondary-state columns
     orthogonal to `lifecycle_phase`.)
  4. `INSERT INTO ff_attempt (partition_key, execution_id,
     attempt_index, ...)` for the new attempt row (initialised
     empty, like a fresh create). The prior terminal `ff_attempt`
     row stays — it is history, not state. Reconciler-scope audit
     is in §4.2.6 (A4): existing scanners key off
     `ff_exec_core.attempt_index = ff_attempt.attempt_index`, so
     historical rows are not picked up. (Post-replay
  `lifecycle_phase = 'runnable'` on the current attempt, as above.)
  5. Outbox: `ff_operator_event` row (`event_type = 'replayed'`).
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
  The loser's pre-state validation (replay requires
  `lifecycle_phase = 'terminal'`; cancel requires
  `lifecycle_phase NOT IN ('terminal','cancelled')`) fails →
  method returns its method-specific `NotReplayable` /
  `AlreadyTerminal`. No corruption.

Net: the lock protocol + CAS fencing make reconciler-interaction
symmetric to Valkey's Lua-atomic behavior. No new reconciler
modifications required.

**Per-attempt scope audit (A4).** `replay_execution` increments
`ff_exec_core.attempt_index` and leaves historical `ff_attempt` rows
in place (§4.2.5 step 4). This adds N-row history semantics to
`ff_attempt` — a new invariant for the backend. Reviewer A flagged
this as a potential scanner hazard. Audited partners:

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
| `replay_execution` | `ff_operator_event` | `replayed` |
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
fields, four have no source column in the existing schema:
`waitpoint_key`, `state`, `required_signal_names`, `activated_at`.
None of those is derivable cleanly:

- `waitpoint_key` — the user-level key string (distinct from the
  UUID `waitpoint_id`); needed by cairn to correlate against the
  worker-side `suspend` call. Not present in any existing table.
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

Revision 3 adds migration 0011 with additive columns:

```sql
ALTER TABLE ff_waitpoint_pending
    ADD COLUMN waitpoint_key         TEXT,
    ADD COLUMN state                 TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN required_signal_names TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN activated_at_ms       BIGINT;
```

All columns are nullable-or-defaulted → additive and
`backward_compatible = true`. Existing rows inserted pre-0011 get
`state = 'pending'`, empty `required_signal_names`, NULL
`waitpoint_key` / `activated_at_ms`. New inserts (post-0011) must
populate `waitpoint_key` + `required_signal_names` from the
producer-side `suspend_*` path; activation transitions populate
`state = 'active', activated_at_ms = now`. The producer + activation
wiring changes land in the same PR as migration 0011 (§6.2 step 4)
— without them the table has usable defaults but degraded fidelity
on already-inserted rows.

**Field mapping (post-0011):**

- `waitpoint_id`, `execution_id`, `created_at` (from
  `created_at_ms`), `expires_at` (from `expires_at_ms`),
  `token_kid` — direct columns.
- `token_fingerprint` — computed from `token` at projection
  (redaction per Stage D1 contract; never returned raw).
- `waitpoint_key`, `state`, `required_signal_names`, `activated_at`
  (from `activated_at_ms`) — direct columns post-0011.

**Pre-0011 row projection (NEW-5).** `PendingWaitpointInfo.waitpoint_key`
is `String` (non-Option). Rows inserted before migration 0011 have
`waitpoint_key IS NULL` and cannot be backfilled without access to
the original producer-side `suspend_*` call site. Projection
behavior: `COALESCE(wp.waitpoint_key, '') AS waitpoint_key`,
surfacing an empty string for pre-0011 rows (legible to cairn as
"unknown" without contract-violating a `null`). Each projected
empty-key row increments a new operator-observable counter
`ff_waitpoint_pending_degraded_total{reason="missing_waitpoint_key"}`
so operators can observe the degraded-legibility tail and age it
out as pre-0011 waitpoints resolve. The counter plumbs through the
existing metrics surface (OTEL via ferriskey per 2026-04-22 owner
lock) — no new telemetry-infra surface. Filtering pre-0011 rows
from the listing was considered and rejected: hiding operator-
visible in-flight suspensions is worse than returning them with a
sentinel + counter.

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
   cross-coupling; unblocks `examples/token-budget` on Postgres.
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
   numbered after 0011); emits on the `ff_operator_event` channel
   landed in step 3.

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
  - `ff_cancel_backlog` + `ff_cancel_backlog_member` (spine-A pt.2
    step, numbered after 0011 in impl-order).
  - `ff_operator_event` outbox table + trigger (migration 0010,
    lands with spine-A pt.1).
  - Additive columns on `ff_waitpoint_pending` (migration 0011,
    lands with standalone-2 step).
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
  end-to-end. It must PASS on the Postgres backend before tag; a
  failure withdraws the release (whole-wave-or-withdraw).
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

### 7.2 Budget counter granularity — **RESOLVED in §4.4**

`report_usage_impl` already ships
(`crates/ff-backend-postgres/src/budget.rs:154`). The granularity
choice is already made in-tree; admin methods commit to match that
shape. Not an open fork.

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

### 7.10 Remaining owner-call items (push if needed)

None. All Round-2 findings resolved in Revision 3 via agent
autonomy (Option X outbox channel, additive waitpoint migration,
atomic-at-release-PR flip, C3a–d). If the owner disagrees with any
resolution (Option X vs Y; additive-migration vs join-derive for
waitpoint; `get_execution_result` semantics; ack silence), §7
reopens the specific item in Round-4.

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

**Rejected.** The schema decisions in Revision 3 remain contained:
join-on-read against existing columns (no new projection tables);
one new backlog pair; one new operator-event outbox channel
(migration 0010); four additive columns on an existing table
(migration 0011). None of these is a forward-facing primitive that
warrants a separate RFC audience — they are all mechanical
prerequisites for the method impls this RFC proposes. If a future
Wave-10 introduces projection tables or cross-cutting schema
shapes, it is welcome to split at that time. RFC-020 stays
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
  `report_usage_impl` (the §4.4 / §7.2 binding shape)
- `crates/ff-backend-postgres/src/reconcilers/lease_expiry.rs` —
  the §4.2.6 race partner
- `crates/ff-backend-postgres/src/reconcilers/attempt_timeout.rs`
  — per-attempt scope audit (A4, §4.2.6)
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
