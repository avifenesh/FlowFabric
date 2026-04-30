# RFC-021 — Recurring scheduled execution

- **Status:** DRAFT
- **Proposed:** 2026-04-26
- **Tracks issue:** #336
- **Depends on:** RFC-009 (scheduling lanes), RFC-012 (`EngineBackend` trait), RFC-017 (ingress rows), RFC-018 (capability discovery)
- **Supersedes:** none

## 1. Summary

A first-class recurring-schedule primitive owned by the engine. A consumer
registers a `ScheduleDef` — a cron expression (5-field POSIX) or an
`@every <duration>` interval, plus an execution template (lane, payload,
policy, caps) — and FlowFabric fires new executions on the cadence, with
leader-elected single-firing across replicas, bounded missed-tick
catch-up, per-schedule metrics, and full cancel/pause/resume control.
Schedules are stored in the same backend as the rest of the engine state
(Valkey hashes + partitioned ZSETs, Postgres row + `next_fire_at` index,
SQLite mirror per RFC-022) and survive process restart. The feature
replaces every consumer-side cron loop driving `create_execution`.

## 2. Motivation

Each of the five use cases below forces the consumer to run its own
scheduling sidecar today. The failure mode is uniform: either the
sidecar dies and nothing fires for hours, or it runs on N replicas and
fires N times per tick. Engine-owned scheduling removes both.

1. **Daily cleanup / retention** — "02:00 UTC nightly, archive 30d+
   executions." Single-node cron = SPOF; multi-node = consumer-side
   leader election (which the engine already has primitives for).
2. **Periodic digests** — "Monday 09:00, weekly summary email."
   Missing a week because the consumer was deploying is silent data
   loss; engine catch-up (§4.5) surfaces + fires it.
3. **Canary / health probes** — "every 30s round-trip." Client
   timers can't share real retry / budget / quota policy, so canary
   failures don't look like real failures. A real execution through
   the real scheduler gives honest signal.
4. **Billing rollups** — "every hour, per-tenant token usage."
   Must not double-fire (double-bill) or skip (lose revenue). The
   schedule's fire composes with `ff_issue_claim_grant` exactly-once.
5. **Third-party sync** — "every 15m, pull upstream deltas." Fire
   cadence + skew + last-fire-age belong on engine dashboards, not a
   sidecar.

The common thread: each is a flow, and a flow's trigger belongs where
flows live. Shipping scheduling as a consumer concern pushes leader
election, missed-tick policy, skew observability, and backend parity
onto every downstream — five re-implementations of the same problem.

## 3. Consumer surface

**Decision: Option C — trait method on `EngineBackend`, exposed via
`FlowFabricAdminClient::register_schedule` and
`POST /v1/schedules` on `ff-server`.**

### 3.1 Surface shape

Six new `EngineBackend` methods (each with `Unavailable` default
impls per RFC-017 Stage A — additive for out-of-tree backends):
`register_schedule`, `cancel_schedule`, `pause_schedule`,
`resume_schedule`, `describe_schedule(id) -> Option<Snapshot>`,
`list_schedules(filter) -> Vec<Snapshot>`. `RegisterScheduleArgs`
carries an `idempotency_key` symmetric with `create_execution` so a
network-blip retry cannot duplicate.

### 3.2 Full args shape

```rust
pub struct RegisterScheduleArgs {
    pub schedule_id: ScheduleId,     // caller-supplied, stable across retries
    pub namespace: Namespace,
    pub cadence: Cadence,            // Cron(String) | Every(Duration ≥ 1s)
    pub template: ExecutionTemplate, // lane, payload, policy, caps, budgets,
                                     // quota_policy_id, optional flow_id
    pub catch_up: CatchUp,           // Skip | FireOne (default) | FireAll
    pub start_at: Option<TimestampMs>, // default: now
    pub end_at: Option<TimestampMs>,   // default: never
    pub max_fires: Option<u64>,        // default: unbounded
    pub idempotency_key: String,       // dedupes register retries
}
```

`Cadence::Every(Duration)` enforces ≥ 1s at ingress (§5 sub-second
non-goal). `ExecutionTemplate::payload: Vec<u8>` is handed verbatim to
`create_execution` at fire time. `flow_id: Option<FlowId>` — set to
attach fires to a parent flow, None for solo executions.

### 3.3 Why Option C

Additive with RFC-017 (trait + `Unavailable` defaults, both backends
override symmetrically); backend-neutral (state in backend, not
consumer memory, so restarts + failover don't lose or double-fire);
composes with the existing HTTP control plane (`POST/DELETE/PAUSE
/v1/schedules/...` symmetric with every other ingress); testable in
isolation via the same trait-mock + FCALL-test pattern the rest of
the engine uses. Options A and B arguments in §6.

## 4. Complete design

### 4.1 Cron dialect

**Decision: 5-field POSIX cron + `@every <duration>` interval shorthand.**

```
┌───────────── minute        (0–59)
│ ┌─────────── hour          (0–23)
│ │ ┌───────── day-of-month  (1–31)
│ │ │ ┌─────── month         (1–12 or JAN–DEC)
│ │ │ │ ┌───── day-of-week   (0–6, Sunday=0, or SUN–SAT)
│ │ │ │ │
* * * * *
```

Supported tokens: `*`, `N`, `N-M` range, `N,M` list, `*/K` step,
month names `JAN..DEC`, day-of-week `SUN..SAT` (case insensitive).
Examples: `0 2 * * *` (02:00 UTC daily), `*/15 * * * *` (every 15m),
`0 9 * * MON` (09:00 UTC Mon), `@every 30s`, `@every 1h30m`.

Rejected at register-time (loud, not silent): **6-field (seconds)
cron** (sub-second is §5 non-goal; `* * * * * *` fails rather than
silently truncating); **Vixie extensions** (`?`, `L`, `W`, `#` —
calendrical-DST interactions or one-DB-specific semantics, keeps the
grammar one page); **named schedules** `@daily` / `@hourly` /
`@reboot` (sugar for 5-field; `@reboot` meaningless in a distributed
replica set; `@every` is the single exception because cron alone
can't express e.g. every 90s).

Parser lives in `ff-core::schedule::cron`, pure-Rust. Choice of
external crate vs hand-rolled is impl-time.

### 4.2 Storage

Schedules are partitioned by a stable hash of `schedule_id` to reuse
the same partition fabric the rest of the engine uses. Every
schedule lives on exactly one partition; a schedule is a member of
its partition's `pending_fire` ZSET keyed by `next_fire_at_ms`.

**Valkey** — the `{s:P}` tag is derived from `schedule_id` via the
shared FNV helper so every per-schedule key lands in the same slot,
mirroring the `{e:N}` co-location property for executions.

```
ff:sched:{s:P}:<schedule_id>:def          HASH (namespace, cadence_kind, cadence_expr,
                                                template_json, catch_up, start_at_ms,
                                                end_at_ms, max_fires, state,
                                                created_at_ms, idempotency_key,
                                                fire_count, last_fire_at_ms,
                                                last_fire_exec_id, last_fire_skew_ms)
ff:sched:{s:P}:pending_fire                ZSET  (member=schedule_id, score=next_fire_at_ms)
ff:sched:{s:P}:paused                      SET
ff:sched:{s:P}:leader                      STRING (SET NX EX, advisory lock)
ff:sched:{s:P}:fire_history:<schedule_id>  STREAM (capped ring, 256 entries)
ff:sched:namespace:<ns>:members            SET    (cross-partition list index)
```

**Postgres:**

```sql
CREATE TABLE ff_schedule (
    schedule_id       TEXT PRIMARY KEY,
    namespace         TEXT NOT NULL,
    partition_idx     SMALLINT NOT NULL,   -- matches Valkey partition for ops parity
    cadence_kind      TEXT NOT NULL,       -- 'cron' | 'every'
    cadence_expr      TEXT NOT NULL,
    template          JSONB NOT NULL,
    catch_up          TEXT NOT NULL,
    start_at          TIMESTAMPTZ,
    end_at            TIMESTAMPTZ,
    max_fires         BIGINT,
    state             TEXT NOT NULL,       -- 'active' | 'paused' | 'cancelled' | 'expired'
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    idempotency_key   TEXT NOT NULL,
    next_fire_at      TIMESTAMPTZ,
    fire_count        BIGINT NOT NULL DEFAULT 0,
    last_fire_at      TIMESTAMPTZ,
    last_fire_exec_id TEXT,
    last_fire_skew_ms BIGINT
);
CREATE UNIQUE INDEX ff_schedule_idempotency
    ON ff_schedule (namespace, idempotency_key);
CREATE INDEX ff_schedule_due
    ON ff_schedule (partition_idx, next_fire_at)
    WHERE state = 'active';

CREATE TABLE ff_schedule_fire_history (
    schedule_id   TEXT NOT NULL,
    fire_seq      BIGINT NOT NULL,
    fired_at      TIMESTAMPTZ NOT NULL,
    execution_id  TEXT NOT NULL,
    skew_ms       BIGINT NOT NULL,
    PRIMARY KEY (schedule_id, fire_seq)
);
```

The history table is TTL'd by the retention scanner at `fire_seq < MAX - 256`.

**SQLite (RFC-022 draft mirror):** same two tables, `TIMESTAMPTZ →
INTEGER ms`, same indexes; WAL mode handles the single-writer
schedule loop.

### 4.3 Tick loop

`ScheduleFireScanner` in `ff-engine::scanner::schedule_fire`,
partition-owning, bounded-slice rotation-cursor probe per tick (same
pattern as `ff_scheduler::Scheduler::claim_for_worker`). For each due
schedule on a partition it owns, it issues a single atomic
`FCALL ff_fire_schedule` (Valkey) / `SELECT ... FOR UPDATE SKIP
LOCKED` transaction (Postgres) that: (1) asserts `state=active` +
caller-is-leader (§4.4); (2) computes next `fire_at` from cadence +
catch_up; (3) calls `create_execution` with the template, stamping
`idempotency_key = <schedule_id>:<fire_seq>`; (4) updates
`fire_count`, `last_fire_at`, `last_fire_exec_id`, skew, and
`next_fire_at` atomically; (5) appends a `fire_history` entry.

**Interval:** scanner runs every 1s (shorter than any legal cadence).
`next_fire_at` drives actual firing; the scanner polls and either
fires (if overdue) or sleeps. Reuses the existing `supervised_spawn`,
`ScannerFilter`, restart-on-panic, and metrics wiring — no new
service process.

### 4.4 Multi-instance safety

**Decision: per-partition advisory lock + fence token.** A replica
attempts `SET NX EX` on `ff:sched:{s:P}:leader` (TTL 10s, refreshed
every 3s) at the start of each partition probe; on success it is the
owner until expiry. Lock value carries a monotonic fence token
(`INCR` on `ff:sched:{s:P}:leader_fence`) which `ff_fire_schedule`
asserts against the live fence before writing, so a stale leader
whose lock expired mid-tick is rejected atomically. Postgres uses
`pg_try_advisory_lock(partition_idx)` held inside the firing
transaction; fence is the txn LSN captured at acquisition.

Why not lock-free rotation (the claim-grant pattern): claim-grant
atomically transitions `eligible → claimed` so racing callers lose
harmlessly, but schedule firing *creates new state*. Without a
leader, N replicas all see `next_fire_at <= now` and all call
`create_execution`. The execution's `idempotency_key` dedupes at the
exec layer, but leaves N-1 rejected create calls per tick per
schedule — wasteful and a steady-state log storm. The lock removes it.

### 4.5 Missed-tick semantics

**Default `FireOne`:** a schedule overdue by ≥ 1 tick fires exactly
one catch-up execution, then resumes cadence. The fired execution is
stamped with `catch_up = true` + `originally_scheduled_at_ms` so the
handler can distinguish fresh from overdue runs.

Per-schedule override modes:

- **`Skip`** — fast-forward `next_fire_at` past the missed window,
  fire nothing. Use for canaries / sensors where a stale sample is
  worse than no sample.
- **`FireAll`** — fire every missed slot, capped at
  `max_catch_up_fires` (default 32, hard max 256, enforced by Lua /
  SQL). Use for billing / accumulators where every window must
  eventually fire. The cap is non-negotiable: uncapped, a schedule
  paused for a year at `@every 1m` would attempt 525,600 fires on
  resume. `FireAll` serialises one fire per scanner tick so a
  32-slot backlog drains in ~32s and per-tick cost stays bounded.

### 4.6 Failure handling

The schedule's responsibility ends at `create_execution`. Retries,
deadlines, budget policy all belong to the execution (the template
already carries `ExecutionPolicy`). `next_fire_at` advances whether
the fired execution later succeeds or fails — a cron that errors
does not cancel tomorrow's run.

Transport failure on `ff_fire_schedule` itself (Valkey/PG down) does
**not** advance `next_fire_at`; next tick retries. A 5s outage
produces 0 fires, and the `CatchUp` policy then governs recovery.

`create_execution` returning `QuotaExceeded` / `BudgetExceeded` is
recorded in `last_fire_error` but **still advances `next_fire_at`** —
otherwise a sustained quota breach pins the schedule, then causes a
`FireAll` flood when the quota recovers. Observable fields on the
schedule: `fire_count`, `fire_error_count`, `last_fire_error`.

### 4.7 Cancellation

`cancel_schedule` transitions `state → cancelled` and removes the
schedule from `pending_fire`. Already-fired executions are untouched
— cancelling does not cascade to prior spawned work. Consumer wanting
cascade calls `cancel_flow` (if the template set `flow_id`) or
filters `list_executions(spawned_by = schedule_id)` (new filter) and
cancels each.

`pause_schedule` / `resume_schedule` toggle membership in the
`paused` set; a paused schedule is skipped by the scanner, and
`resume` applies the configured `CatchUp` policy across the paused
interval. `cancel_schedule` soft-deletes with 30d audit retention;
hard deletion is the retention scanner's job.

### 4.8 Observability

Metrics (dimensioned on `namespace` + `schedule_id_hash`, not raw
ID, to bound cardinality): `ff_schedule_fires_total{result}`
counter, `ff_schedule_fire_skew_ms` histogram (`now -
scheduled_fire_at`), `ff_schedule_active_count{state}` gauge,
`ff_schedule_catch_up_fires_total` counter,
`ff_schedule_leader_acquire_total{outcome}` +
`ff_schedule_leader_lost_total` counters.

Tracing: `schedule.fire` parent span wraps the full `ff_fire_schedule
→ create_execution` chain; the fired execution's root span is its
child so an operator walking an execution trace can reach the
schedule that fired it.

Per-schedule fire history (§4.2 stream/table, last 256 fires with
timestamp + execution ID + skew) is the consumer-visible audit log,
exposed via `describe_schedule`.

### 4.9 Timezone handling

**Decision: UTC only. DST-awareness is a permanent §5 non-goal, not a
deferral.**

A cron expression is interpreted in UTC; `0 9 * * MON` fires at 09:00
UTC. A consumer wanting local-time behaviour computes the UTC offset
itself. IANA `tz` identifiers and DST-aware local-time cron are
rejected — both introduce the spring-forward / fall-back ambiguity
(2:30 AM missing, 1:30 AM duplicated) that is the single largest
source of scheduler bugs in every system that's tried them, and both
add a tz-database dep to every backend. If a later RFC decides
timezone cron is worth those costs, it would add a new
`Cadence::CronTz { expr, tz }` variant — additive to this feature,
not a revision. This RFC's scope is UTC and that is the whole scope.

### 4.10 Parity

| Concern                | Valkey                           | Postgres                                        | SQLite (RFC-022)              |
|------------------------|----------------------------------|-------------------------------------------------|-------------------------------|
| Schedule def           | `ff:sched:{s:P}:<id>:def` HASH   | `ff_schedule` row                               | `ff_schedule` row             |
| Due index              | `ff:sched:{s:P}:pending_fire` ZSET | `ff_schedule_due` partial index                | same partial index            |
| Paused index           | `ff:sched:{s:P}:paused` SET      | `state='paused'` in `ff_schedule`               | same                          |
| Leader lock            | `SET NX EX` + fence              | `pg_try_advisory_lock(partition_idx)`           | single-writer (WAL)           |
| Fire atom              | `FCALL ff_fire_schedule`         | `BEGIN; SELECT FOR UPDATE; INSERT; UPDATE; COMMIT` | same transaction            |
| Fire history           | STREAM, capped 256 per schedule  | `ff_schedule_fire_history` table                | same table                    |
| Idempotency (register) | Key collision on HSETNX          | Unique index `ff_schedule_idempotency`          | same                          |
| Idempotency (fire)     | Execution `idempotency_key`      | Execution `idempotency_key`                     | same                          |
| Scanner                | `ScheduleFireScanner` + rotation | `PgScheduleFireScanner` + `FOR UPDATE SKIP LOCKED` | single-writer poll loop     |

All three backends MUST be fire-complete (`register/cancel/pause/
resume/describe/list/fire`) before the feature is released. SQLite
ships alongside RFC-022; if RFC-022 slips, RFC-021 slips to the same
tag.

### 4.11 Examples

A new `examples/scheduled-flows/` ships with the feature (§9 gate
4). The headline registers two schedules — a daily cleanup
(`Cadence::Cron("0 2 * * *")` + `CatchUp::FireOne`) and a 30s
canary (`Cadence::Every(30s)` + `CatchUp::Skip`) — live-runs against
both Valkey and Postgres, emits visible fire events, then
demonstrates `cancel_schedule` + `pause_schedule` + `resume_schedule`
and asserts on observed fire counts. ~150-200 LOC per CLAUDE.md §5
gate 4.

## 5. Non-goals

Permanent, not deferred:

- **Sub-second precision.** Minimum cadence 1s; `@every 500ms` is
  rejected. FlowFabric is an execution engine, not a real-time
  scheduler. Sub-second periodic work runs inside a handler.
- **Event-driven schedules.** Time-based only. Fire-on-signal /
  fire-on-stream / fire-on-completion is RFC-005 / RFC-014
  territory and already expressible via `suspend` + signal delivery.
- **General-purpose cron service.** FlowFabric schedules fire
  FlowFabric executions, not arbitrary HTTP / shell. A consumer
  wanting webhooks writes a one-line handler that POSTs.
- **Timezone / DST-aware cron.** UTC only, §4.9. A later timezone
  RFC would add a new cadence variant, not revise this feature.
- **Flow-scoped cron with cascade-cancel.** Schedules are
  namespace-scoped. "When parent flow completes, stop firing" is
  expressible via an explicit `cancel_schedule` from the parent's
  completion handler.
- **`@reboot` / startup-only triggers.** Meaningless in a
  distributed replica set; consumers run startup work in their own
  process.

## 6. Alternatives rejected

### 6.1 Option A — `flow.run_every(cron_expr, handler)` at flow-definition time

Reads like client-side cron libraries. Rejected because FlowFabric
has no flow-definition DSL — flows are created imperatively via
`create_flow` + `add_execution_to_flow`. Retrofitting `run_every`
needs either a parallel DSL (two ways to create a flow, duplicate
doc surface) or a declarative-flow engine rewrite tracked nowhere.
Option C gets the same ergonomics via the existing admin surface.

### 6.2 Option B — server config registers schedules

Schedules declared in `ff-server.toml`, loaded at boot. Rejected:
schedules are data, not config — they are created / paused /
cancelled at runtime, and reloading a config file to cancel one is
an ops anti-pattern. Multi-tenant consumers can't share a config
file. Option C is strictly additive: a config file CAN seed
schedules at boot by calling the HTTP API from a startup script; the
inverse (adding ingress APIs to a config-declared surface) is much
harder.

### 6.3 Consumer-side cron loop

"Run `tokio-cron-scheduler` in the consumer." Rejected: runs in
consumer memory (deploy / crash / scale-to-zero kills it silently);
N-replica cron requires consumer-implemented leader election (we
have yet to see one done correctly); FlowFabric sees the execution
but not the missed fire or skew spike. This is the current state —
§2 is a catalog of its failure modes.

### 6.4 External cron daemon triggers a run endpoint

"Use `systemd timer` / `k8s CronJob` to curl `ff-server`." Rejected:
`systemd timer` has no distributed story; `k8s CronJob` has one but
ships documented double-firing edge cases (concurrency policy is
best-effort, not exactly-once). `startingDeadlineSeconds` fires
every missed instance within the window — equivalent to uncapped
`FireAll`, wrong default for most use cases with no per-schedule
override. Payload construction in shell loses the type-safety the
Rust API provides. Same observability split as 6.3.

## 7. Open questions

Three forks genuinely need owner input before acceptance. Each has
an author lean (not a decision). None expand RFC-021's scope — they
pick points inside the drawn boundary.

1. **Per-flow vs per-namespace `ScheduleId` scoping.** Schedules are
   namespace-scoped in the current draft. Should an additional index
   scope them under parent `FlowId` for cheap "all schedules feeding
   flow X" queries? *Lean: namespace-only; add
   `ff:sched:flow:<flow_id>:members` secondary index only if the
   listing use case materialises.* Key-shape decision needs owner
   lock before impl.

2. **Multi-schedule fan-out.** Current draft: 1 schedule = 1
   template = 1 fire per tick. Fan-out variant: 1 schedule = 1
   template-factory = N fires per tick (e.g. "every hour, for each
   of 500 tenants"). *Lean: fan-out is out of scope; the consumer
   registers 500 schedules.* But 10K+ tenants makes per-tenant
   registration ugly. Need owner call on whether that scale is
   real, because it decides whether the scanner is O(schedules) or
   O(schedules × fanout).

3. **Cron dialect precision.** §4.1 picks 5-field POSIX + `@every`.
   Two sub-questions the parser surface depends on: (a) accept
   `@cron <expr>` prefix (*lean: no*); (b) accept combined
   list+range in one field like `1-5,10,15-20` (*lean: yes, full
   POSIX*). Parser-impl detail, but owner should lock the grammar
   before code commits to it.

## 8. Migration impact

**Purely additive. No breaking changes. No deprecations.** RFC-021
adds: 6 new `EngineBackend` trait methods (each with `Unavailable`
default impls per RFC-017 convention — out-of-tree backends keep
compiling); 2 new Valkey partition-family key namespaces
(`ff:sched:{s:P}:*`, `ff:sched:namespace:*`); 2 new Postgres tables
via migration `0010_schedules.sql`; 2 new SQLite tables via the
RFC-022 schema; 1 new Lua function (`ff_fire_schedule`); 1 new
scanner (`ScheduleFireScanner`, supervised via `supervised_spawn`,
respects `ScannerFilter`); 1 new REST family (`/v1/schedules/*`); 1
new metrics family (`ff_schedule_*`). RFC-009's existing
`enqueue_scheduled` single-delayed-execution shape is untouched and
coexists.

## 9. Release readiness

All of the following must be true before tag. Any failure slips the
release (CLAUDE.md §5):

- **Three-backend parity.** Valkey + Postgres + SQLite (RFC-022)
  all ship the full surface in the same tag — no "Valkey first,
  Postgres next release."
- **Example lives and live-runs.** `examples/scheduled-flows/`
  compiles and the README transcript matches real output on both
  Valkey and Postgres (CLAUDE.md §5 gates 3+4).
- **Tests:**
  - Tick accuracy: `@every 1s` fires within 1500ms of each second
    boundary (p99).
  - Catch-up correctness: paused + resumed schedule fires exactly
    the count each `CatchUp` mode specifies, for a 10-tick gap.
  - Failover: killing the leader mid-tick produces zero
    double-fires; a new leader takes over within 15s.
  - Multi-instance safety: 5 racing replicas × 100 schedules ×
    `@every 1s` produce exactly N fires in N seconds across the
    fleet.
  - Backend parity: every test above passes on all three backends.
- **Docs:** `docs/CONSUMER_GUIDE_SCHEDULES.md` (cadence syntax,
  template semantics, catch-up modes, worked examples),
  `docs/OPERATOR_GUIDE_SCHEDULES.md` (metrics, skew expectations,
  leader-lock TTL tuning, recovery playbooks),
  `POSTGRES_PARITY_MATRIX.md` rows for each new trait method.
- **CHANGELOG** `### Added` entries under the landing version.
- **Smoke gate** `scripts/smoke-schedules.sh`: register `@every 2s`,
  wait 10s, assert ≥ 4 fires + `fire_count == executions_observed`,
  cancel, assert no further fires — both backends, in release.yml
  pre-publish smoke (CLAUDE.md §5 gate 8).

## 10. References

- Issue **#336** — tracking issue for this RFC.
- **RFC-009 Scheduling** (archived) §8.3 explicitly punts recurring
  creation; this RFC fills that gap.
- **RFC-012** engine-backend trait — the pattern this RFC extends.
- **RFC-017** ingress surface — the `Unavailable` default-impl stage
  pattern this RFC reuses.
- **RFC-018** capability discovery — the `ScannerFilter` the new
  scanner respects.
- **RFC-022** SQLite backend (draft) — the third backend this feature
  ships against.
- `flowfabric-archive/rfcs-drafts/v2-deferral-backlog.md §A item #4`
  — the backlog entry this RFC closes.
