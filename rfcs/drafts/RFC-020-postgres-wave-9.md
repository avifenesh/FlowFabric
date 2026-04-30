# RFC-020: Postgres Wave 9 — deferred backend impls

**Status:** DRAFT
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Target release:** v0.11+ (scope-shaping only; NOT a v0.10 release gate)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction + staged Postgres parity), RFC-018 (capability discovery), RFC-019 (stream-cursor subscriptions)
**Related artifacts:**
- `docs/POSTGRES_PARITY_MATRIX.md` — authoritative per-method status
- `crates/ff-backend-postgres/tests/capabilities.rs:53-73` — asserted false-flag list
- `crates/ff-backend-postgres/src/lib.rs` — current `EngineError::Unavailable` sites
- Valkey reference impls in `crates/ff-backend-valkey/src/*`

> **Draft status:** this RFC consolidates scope only. It is NOT accepted.
> Sequencing, design forks, and sub-issue breakouts are open for review.

---

## 1. Summary

The Postgres backend shipped behind the `EngineBackend` trait in v0.7 (RFC-017)
and reached core hot-path parity with Valkey in v0.9. A small, deliberately
bounded set of trait methods — the **Wave 9** surface — still returns
`EngineError::Unavailable { op }` on Postgres. Every one of them is
admin-surface, read-model-shape, or new-table-machinery work; none is on the
create/dispatch/claim hot path. This RFC enumerates the deferred methods
in six coherent groups, sketches the Postgres design shape per group, and
proposes a landing sequence. It does not re-open any trait-surface decisions
from RFC-012 / RFC-017 / RFC-018.

---

## 2. Motivation

### 2.1 Why these were deferred

The RFC-017 staged migration (Stages A–E4) had a single forcing constraint:
ship Postgres as a real backend without regressing Valkey semantics, on a
session-bounded cadence. Every Stage boundary was a CI gate. Wave 9 is the
set of methods where **at least one of the following was true** and the
migration chose to ship a structured `Unavailable` over a rushed impl:

1. **New table machinery required.** e.g. `cancel_flow_header` /
   `ack_cancel_member` need a `ff_cancel_backlog` table; no analogue existed
   pre-RFC-017.
2. **Read-model shape was an open design fork.** e.g.
   `read_execution_info` / `read_execution_state` can be served either by
   a denormalised projection table (fast reads, write amplification) or
   join-on-read against `exec_core` + children (simpler schema, higher
   read cost). Picking one silently under Stage-E2 time pressure would
   have locked in the wrong answer.
3. **Admin-surface semantics are not hot-path.** The operator-control
   family (`cancel_execution`, `change_priority`, `replay_execution`,
   `revoke_lease`) and the budget/quota admin family are called at human
   latencies from cairn's operator UI; the cost of `Unavailable` here is
   bounded (visible 503 vs. silent wrong answer), and cairn consumes
   capabilities via RFC-018 so the UI greys out unsupported actions.

### 2.2 Why these need to land now

- **Consumer parity story.** The cairn migration guide
  (`docs/MIGRATIONS.md` § v0.8 → `FF_BACKEND=postgres`) lists every
  Postgres `Unavailable` surface as a "known gap". Each open gap is a
  cairn-release pin at the ff-server version that still ships it.
- **Capability-matrix drift pressure.** RFC-018 Stage B wants the
  parity matrix rendered from in-code capability values; every `stub`
  row is a line item in that render, and the render is most useful
  once the set is closing rather than growing.
- **Single-backend promise.** An operator who picks
  `FF_BACKEND=postgres` today gets a functional backend with a known
  short list of 503-returning HTTP routes. Wave 9 closes that list and
  lets us drop the "backend-dependent feature gaps" section of the
  consumer docs.

---

## 3. Scope — six groups

The following groups are **authoritative** for Wave 9 scope. Every row
in `docs/POSTGRES_PARITY_MATRIX.md` currently marked `stub` on the
Postgres column falls into exactly one group; every `false` flag in
`crates/ff-backend-postgres/tests/capabilities.rs:53-73` is covered.

### Group 1 — Flow cancel split

| Method | Current Postgres state | Valkey reference |
|---|---|---|
| `cancel_flow_header` | `EngineError::Unavailable` | `ff_cancel_flow` FCALL + `CancelFlowHeader::AlreadyTerminal` idempotent replay |
| `ack_cancel_member`  | `EngineError::Unavailable` | `ff_ack_cancel_member` = `SREM pending_cancels` + conditional `ZREM cancel_backlog` |

Requires a new `ff_cancel_backlog` table (or equivalent) holding the
per-flow pending-members set plus the flow-level cancellation policy +
reason. Bulk `cancel_flow` already ships via Stage E3's reconciler
path; this split unblocks the header/ack dispatcher pattern the
Server uses for sync + async cancel fanout.

### Group 2 — Read model

| Method | Current Postgres state | Valkey reference |
|---|---|---|
| `read_execution_info`   | `EngineError::Unavailable` | `HGETALL exec_core` + StateVector parse |
| `read_execution_state`  | `EngineError::Unavailable` | `HGET exec_core public_state` + JSON deserialize |
| `get_execution_result`  | `EngineError::Unavailable` | `GET ctx.result()` binary-safe |

The three read methods share a schema decision (design fork, §7). Once
the projection shape is fixed, all three are straightforward SELECTs.

### Group 3 — Operator control

| Method | Current Postgres state | Valkey reference |
|---|---|---|
| `cancel_execution`   | `EngineError::Unavailable` | HMGET pre-read + `ff_cancel_execution` FCALL |
| `change_priority`    | `EngineError::Unavailable` | `ff_change_priority` ff-script helper |
| `replay_execution`   | `EngineError::Unavailable` | `ff_replay_execution` + skipped-flow-member variant |
| `revoke_lease`       | `EngineError::Unavailable` | `ff_revoke_lease` + `RevokeLeaseResult::AlreadySatisfied { reason }` |

Each is a per-execution admin op. All four mutate `exec_core` +
`attempt_current` (or equivalents) inside a SERIALIZABLE tx; none
requires new table machinery beyond what Stage E3's reconcilers already
introduced. Expected shape: one SERIALIZABLE function per op with
explicit lost-update detection.

### Group 4 — Budget / quota admin

| Method | Current Postgres state | Valkey reference |
|---|---|---|
| `create_budget`        | `EngineError::Unavailable` | `ff_create_budget` FCALL |
| `reset_budget`         | `EngineError::Unavailable` | `ff_reset_budget` FCALL |
| `create_quota_policy`  | `EngineError::Unavailable` | `ff_create_quota_policy` FCALL |
| `get_budget_status`    | `EngineError::Unavailable` | 3× HGETALL (definition + usage + limits) |
| `report_usage_admin`   | `EngineError::Unavailable` | `ff_report_usage_and_check` without worker handle |

Budget + quota already ships the hot-path `report_usage` on Postgres
(see `crates/ff-backend-postgres/src/lib.rs:823-831` — the worker-side
report). Wave 9 adds the admin surface on top of the existing
`budget.rs` module. This group stands alone — no cross-group coupling.

### Group 5 — Waitpoints

| Method | Current Postgres state | Valkey reference |
|---|---|---|
| `list_pending_waitpoints` | `EngineError::Unavailable` | SSCAN + 2× HMGET + §8 schema + `after`/`limit` pagination |

One enumeration method over pending HMAC-token waitpoints. On Postgres
this is a straight `SELECT … FROM ff_waitpoint … WHERE status = 'pending'
AND id > $1 ORDER BY id LIMIT $2`. Requires the schema-rewrite response
shape landed in Stage D1 (HMAC redaction, `token_kid`, `token_fingerprint`);
that shape is already on the trait.

### Group 6 — Subscribe-instance-tags (separately deferred, #311)

| Method | Current state | Notes |
|---|---|---|
| `subscribe_instance_tags` | `EngineError::Unavailable` on both backends | `n/a` in matrix per #311 audit |

Not technically a Postgres-only gap — both backends return `Unavailable`.
Audited 2026-04-24: cairn's one-shot `instance_tag_backfill` pattern is
served by `list_executions` + `ScannerFilter::with_instance_tag`
pagination, and a realtime tag-churn stream is speculative demand.
Included here only so the capability-test false-flag set is fully
mapped; **Wave 9 does not implement it**. The trait method is
reserved for future concrete demand.

### Not in Wave 9 (already shipped, documentation lag)

- `rotate_waitpoint_hmac_secret_all` — **ships today** via
  `signal::rotate_waitpoint_hmac_secret_all_impl` (`lib.rs:840-853`).
  Capabilities test asserts `true` (`capabilities.rs:44`). The
  v0.8.0 matrix row listing it as Wave 9 is stale; a separate
  housekeeping PR should flip the row to `impl`. **Flagged for
  owner review (§8).**

---

## 4. Design sketches

These sketches are intentionally shallow — they fix the SQL/outbox
shape, not the full schema. Migration DDL lands in the group-specific
PRs.

### 4.1 Group 1 sketch — cancel-backlog table

```sql
-- 00NN_cancel_backlog.sql
CREATE TABLE ff_cancel_backlog (
    flow_id       BYTEA PRIMARY KEY,
    policy        SMALLINT NOT NULL,   -- CancellationPolicy enum
    reason        TEXT     NOT NULL,
    requested_at  BIGINT   NOT NULL,
    pending_members BYTEA[] NOT NULL
);
```

- `cancel_flow_header` = SERIALIZABLE tx: read flow terminal state
  from `exec_core` (members); if already terminal return
  `AlreadyTerminal { policy, reason, members }`; else INSERT the
  backlog row + bulk flip member `public_state` to `Cancelling`.
- `ack_cancel_member` = SERIALIZABLE tx: `UPDATE ... SET pending_members
  = array_remove(pending_members, $1); DELETE ... WHERE
  array_length(pending_members, 1) IS NULL` (single statement via CTE).

The Valkey impls' idempotency contract (`CancelFlowHeader::AlreadyTerminal`
surfaces stored policy/reason + full member list) translates cleanly to
the backlog row.

### 4.2 Group 2 sketch — read model

Two forks (see §7). Regardless of fork:

- `read_execution_state` is a single-column `SELECT public_state FROM
  exec_core WHERE id = $1`. Trivial under either fork.
- `read_execution_info` is the composite — all seven StateVector
  sub-states + timestamps. Either a denormalised
  `ff_execution_view` projection (updated via trigger or explicit
  write from the SERIALIZABLE hot-path) or a multi-join SELECT.
- `get_execution_result` is `SELECT result FROM exec_result WHERE
  execution_id = $1 AND attempt_index = $2` against a small
  result-store table keyed by `(execution_id, attempt_index)`;
  insert happens from the attempt-complete path in `attempt.rs`.

`LISTEN/NOTIFY` is **not** required for the read path — these are
point-read APIs, not subscriptions. (RFC-019 already covers
subscription-shaped reads.)

### 4.3 Group 3 sketch — operator control

One SERIALIZABLE function per op; all four follow the same template:

1. `SELECT ... FOR UPDATE` on `exec_core` + `attempt_current` to
   capture the pre-state.
2. Validate against the Valkey-canonical semantics (e.g.
   `revoke_lease` returns `AlreadySatisfied { reason: "no_active_lease" }`
   when `current_worker_instance_id` is empty).
3. `UPDATE` with explicit lost-update fencing (compare-and-set on
   `attempt_index` / `current_worker_instance_id`).
4. Emit the corresponding outbox row (`ff_lease_event` for
   lease-affecting ops) so RFC-019 subscribers see the event.

`replay_execution`'s skipped-flow-member variant (Valkey uses SMEMBERS
over the flow partition's incoming-edge set) maps to a CTE over
`ff_flow_edge` — no new machinery.

### 4.4 Group 4 sketch — budget / quota admin

All five methods sit on top of the existing `budget.rs` module:

- `create_budget` / `reset_budget` / `create_quota_policy` = single
  `INSERT ... ON CONFLICT DO UPDATE` against `ff_budget` /
  `ff_quota_policy` tables (existing, from migration `0002_budget.sql`).
- `get_budget_status` = 3× SELECT (definition + usage + limits) +
  `NotFound { entity: "budget" }` on empty row set, mirroring Valkey's
  3× HGETALL pattern.
- `report_usage_admin` = worker-handle-less variant of the existing
  `report_usage` hot path; factor out the shared
  `report_usage_and_check_core` helper and call with a sentinel
  `worker_instance = None`.

### 4.5 Group 5 sketch — list pending waitpoints

```sql
SELECT id, kid, fingerprint, created_at, flow_id, execution_id, attempt_index
FROM ff_waitpoint
WHERE status = 'pending' AND id > $after
ORDER BY id
LIMIT $limit;
```

Map rows to `PendingWaitpointInfo` (the Stage-D1 schema shape with HMAC
redaction + `token_kid` + `token_fingerprint`). No new table required —
`ff_waitpoint` already exists from `0004_suspend_signal.sql`.

---

## 5. Non-goals

1. **Not re-litigating the trait surface.** RFC-012 locked the
   higher-op trait philosophy; RFC-017 locked the staged-migration
   shape; RFC-018 locked the capability-discovery shape. Wave 9 is
   implementation on top of those accepted decisions.
2. **Not changing Valkey behavior.** Every group translates existing
   Valkey-canonical semantics into Postgres SQL. Where a semantic
   feels under-specified on Valkey (§7.4), Wave 9 treats that as
   an owner-review fork and does NOT quietly pick.
3. **Not introducing new consumer-facing APIs.** The trait methods
   are already public; Wave 9 only changes the inner `Unavailable`
   to a real impl. No new crate-public types, no new HTTP routes.
4. **Not a release gate for v0.10.** v0.10 ships with capabilities
   reshape + `suspend_by_triple` + the RFC-019 subscription surface.
   Wave 9 is v0.11+ scope; no consumer migration pressure forcing a
   faster cadence.
5. **Not closing #311.** `subscribe_instance_tags` remains
   intentionally-deferred on both backends; Wave 9 inventories it
   but does not implement it.

---

## 6. Sequencing

Proposed landing order — each group is an independent PR. The owner
may re-order; the only hard dependency is Group 2's design-fork
resolution (§7.1) before Group 3's outbox events wire up.

1. **Group 5 (`list_pending_waitpoints`)** — smallest first win. One
   method, existing table, deterministic SQL. Good shakedown for the
   Wave-9 PR template.
2. **Group 4 (budget / quota admin)** — five methods, existing tables,
   existing module. No cross-group coupling.
3. **Group 1 (flow cancel split)** — one new table, two methods.
   Unblocks the matrix's `cancel_flow_header`/`ack_cancel_member`
   rows and lets the Server stop branching on backend for the
   sync/async cancel dispatcher.
4. **Group 2 (read model)** — design fork resolves (§7.1), then
   three methods land together. Blocks the HTTP smoke on
   `GET /v1/executions/{id}` family.
5. **Group 3 (operator control)** — four methods. Depends on Group 2's
   read-model shape only for the response projections; the mutating
   paths are independent. Last because it's the largest single group
   and benefits from Group 1's cancel-backlog patterns being in-tree.
6. **Group 6** — no-op; matrix + capability-test documentation update
   only, unless §7.6 resolves in favour of implementation.

**Smallest first win:** Group 5. Roughly 80 lines of impl + a
partition-aware pagination test.

---

## 7. Open questions (design forks per group)

### 7.1 Read-model shape (Group 2)

**Fork:** denormalised `ff_execution_view` projection table (fast
point reads, extra writes on every state transition) vs.
join-on-read against `exec_core` + `attempt_current` + friends
(simpler schema, more expensive reads). The Valkey side is
trivially a flat hash read — this is a Postgres-native decision.
Cairn's operator UI is the primary consumer; its call pattern is
"one execution at a time, on operator click" — not a scan. That
argues for join-on-read. But the StateVector shape touches seven
sub-states, some of which live in separate tables today (e.g.
flow membership in `ff_flow_edge`); a projection table avoids the
join fanout.

**Owner decision needed.** Flagging rather than picking.

### 7.2 Budget counter granularity (Group 4)

**Fork:** per-partition budget counters (scales write-side, needs
periodic aggregation for `get_budget_status`) vs. global counters
(single hot row under contention, no aggregation). Valkey ships
per-partition via the `{budget:b:<id>:{p:N}}` hash-tag pattern.
Postgres does not have the per-partition Lua-atomicity driver that
shaped the Valkey choice; a single `ff_budget_usage` row with
`UPDATE ... WHERE version = $1` compare-and-set may be simpler.

**Owner decision needed.**

### 7.3 Operator-control outbox emission (Group 3)

Should `cancel_execution` / `change_priority` / `replay_execution`
/ `revoke_lease` emit synthetic entries on the lease-event outbox?
Valkey emits via the Lua paths that already XADD to
`ff:part:{fp:N}:lease_history`. Postgres parity would require
explicit `INSERT INTO ff_lease_event ... ` inside each SERIALIZABLE
function. Not doing so would break RFC-019 subscribers who rely on
these events. Preferred answer: yes, emit — but flagging explicitly
because it affects the SQL shape.

### 7.4 `revoke_lease` semantic ambiguity

The Valkey impl returns `RevokeLeaseResult::AlreadySatisfied { reason:
"no_active_lease" }` when `current_worker_instance_id` is empty — a
Stage-C call-out notes this is a "minor delta vs pre-migration HTTP
404". Is the Postgres impl to match the Valkey Lua-canonical result,
or the pre-migration HTTP 404? **Owner fork.** RFC drafts assumes
match-Valkey per Non-goal §5.2.

### 7.5 Cancel-backlog storage shape (Group 1)

`pending_members BYTEA[]` (array column, single row per flow) vs. a
junction table (`ff_cancel_backlog_member (flow_id, member_id)`). The
array is simpler for the two operations in scope. A junction table
scales better if future scope adds per-member status / timing. Wave
9 leans array; owner may overrule.

### 7.6 Group 6 revisit

Is there any emerging consumer pressure to ship
`subscribe_instance_tags` as a real stream, or is the #311 deferral
still correct? Default: keep deferred. Flagging only because this
RFC enumerates the entire `Unavailable` set and a future reviewer
might otherwise assume an oversight.

### 7.7 Ambiguity surfaced for owner: `rotate_waitpoint_hmac_secret_all`

Already shipped (see §3 "Not in Wave 9"). Parity matrix v0.8.0 row
is stale. A separate housekeeping PR should flip the row to `impl`;
this RFC does not include that change (Non-goal: no parity-matrix
changes at draft time).

---

## 8. Alternatives rejected

### 8.1 Close-by-attrition (per-method issues, no unifying RFC)

Rejected: cross-group design forks (§7.1 read-model shape, §7.2
budget granularity, §7.3 outbox emission) affect multiple methods.
Filing six independent issues loses the cross-group review surface
and risks inconsistent answers across groups. An RFC lets the
design forks be adjudicated once.

### 8.2 Ship without RFC, per-method PRs

Rejected: Wave 9 is broad enough (14 methods across 6 groups) that
scope creep is a real risk. "I'm already touching `budget.rs`,
let me also refactor X" is the exact shape that caused CHANGELOG
batch conflicts in the v0.3.x release cycle. An RFC with locked
scope + sequencing + non-goals prevents that drift.

### 8.3 Collapse Wave 9 into v0.10

Rejected: v0.10 already shipped (2026-04-26). Wave 9 is v0.11+
scope. The capability-discovery reshape + `suspend_by_triple`
already used the v0.10 budget; squeezing 14 additional methods
would have tripped the session-bounded cadence.

---

## 9. References

- `docs/POSTGRES_PARITY_MATRIX.md` — authoritative per-method status, v0.8.0 parity summary table rows
- `crates/ff-backend-postgres/tests/capabilities.rs:53-73` — asserted false-flag list, the Wave-9 scope contract
- `crates/ff-backend-postgres/src/lib.rs` — current `Unavailable` sites (grep `EngineError::Unavailable`)
- `crates/ff-backend-valkey/src/*` — Valkey reference impls for semantic parity
- `rfcs/RFC-012-engine-backend-trait.md` §Round-7 — higher-op trait philosophy
- `rfcs/RFC-017-ff-server-backend-abstraction.md` §9 — staged-migration structure, Stage E4 Wave-9 deferral notes
- `rfcs/RFC-018-backend-capability-discovery.md` §4 — `Supports` struct shape
- `rfcs/RFC-019-stream-cursor-subscription.md` — outbox + `LISTEN/NOTIFY` pattern (basis for §4.3 outbox emission)
- Issue #311 — `subscribe_instance_tags` deferral audit
- Issue #277 — cairn operator-UI capability-discovery driver
