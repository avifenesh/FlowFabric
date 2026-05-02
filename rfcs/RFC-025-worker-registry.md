# RFC-025: Worker registry — backend-agnostic trait methods

**Status:** DRAFT (Revision 4)
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-05-02
**Target release:** v0.14 content delivery (post-weekend v0.14 ergonomics work)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-018 (capability discovery), RFC-019 (lease events), RFC-023 (SQLite dev-only backend)
**Tracking issue:** #473
**Consumer report:** avifenesh/cairn-rs → `PostgresControlPlane` currently stubs these four methods with `unimplemented!("PR-C4")`; cairn-app worker-registry code paths are Valkey-gated today

---

## 1. Problem

Cairn's `Engine` trait includes four worker-registry methods FF has no trait equivalent for:

- `register_worker(worker_id, capabilities)`
- `heartbeat_worker(worker_id, timestamp)`
- `mark_worker_dead(worker_id, reason)`
- `list_expired_leases(now_ms)`

On Valkey, cairn reaches past `EngineBackend` with raw FCALL / hash writes. On Postgres + SQLite, no equivalent storage exists. Cairn's PR-C4 stubs these with `EngineError::Unavailable`; cairn-app's worker-registry code paths are Valkey-gated, so there is no user-visible regression today — but **any second orchestrator, or any future cairn-app feature that needs cross-backend worker liveness, hits the same cliff**.

## 2. Scope decision (what this RFC locks in)

**Worker lifetime, worker stats, and anything else that involves workers are FF-owned.** Every `EngineBackend` implementation ships the four methods with real bodies. Consumer orchestrators (cairn today, future peers) call them through the trait and get identical semantics per backend.

The alternative — consumer-layer worker registries — is explicitly rejected: it permanently scatters orchestrator implementations across bespoke Valkey/PG/SQLite code, breaks the RFC-012 agnosticism thesis for anything worker-facing, and guarantees the next orchestrator repeats cairn's journey.

## 3. What FF already owns on Valkey (ground truth)

The relevant state already lives in Valkey, maintained by the SDK preamble and the scheduler:

| Key | Shape | Writer | Reader |
|---|---|---|---|
| `ff:worker:{instance_id}:alive` | SET NX PX with TTL = `2 × lease_ttl_ms` | `ff_sdk::valkey_preamble::run` on `FlowFabricWorker::connect` (crates/ff-sdk/src/valkey_preamble.rs:88) | duplicate-instance guard at connect |
| `ff:worker:{instance_id}:caps` | CSV of sorted-deduped capability tokens | preamble (same file, SET on connect; DEL on disconnect) | `ff-engine::scanner::unblock::load_worker_caps_union` (crates/ff-engine/src/scanner/unblock.rs:742) |
| `ff:idx:workers` | SADD index of instance ids | preamble | unblock scanner (SSCAN → per-id GET) |
| `ff:idx:{p:N}:lease_expiry` | ZSET, score = `expiry_at_ms` | Valkey Lua (`ff_claim_execution`, `ff_renew_lease`, etc., one entry per partition) | ff-engine's lease-reclaim scanner |

**Implication:** the Valkey impl of all four trait methods is near-free — re-exposes primitives the SDK + scheduler already write to. The work is on Postgres + SQLite (fresh schema + body).

## 4. Proposed trait surface

Added to `ff_core::engine_backend::EngineBackend`, all four gated behind an existing feature set (tentatively `core` — narrow to `suspension` / new `worker-registry` feature during rev-2 based on lease-expiry coupling with suspend state):

```rust
/// Args for `register_worker`.
#[non_exhaustive]
pub struct RegisterWorkerArgs {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    /// Workers serve one-or-more lanes; BTreeSet for stable iteration
    /// + dedup. A worker advertising `{"default","build"}` is
    /// addressable from both lanes' claim queues. Rev-3 fix: was
    /// `lane_id: LaneId` in Rev-1 — single-lane forced N round-trips
    /// per multi-lane worker.
    pub lanes: BTreeSet<LaneId>,
    pub capabilities: BTreeSet<String>,
    /// Liveness TTL. Stored alongside the registration so
    /// `heartbeat_worker` refreshes to the same value without the
    /// caller re-supplying it.
    pub liveness_ttl_ms: u64,
    /// Opaque `namespace` — isolates multi-tenant deployments.
    pub namespace: Namespace,
    pub now: TimestampMs,
}

/// Result of `register_worker`. Registration is idempotent:
///   * `Registered` — no prior live key for this `worker_instance_id`
///     (fresh boot or post-TTL-expiry).
///   * `Refreshed` — existing live key was found; TTL reset +
///     caps/lanes overwritten (in-process hot-restart).
/// Re-registering with the same `worker_instance_id` under a
/// DIFFERENT `worker_id` is rejected with
/// `Validation(InvalidInput, "instance_id reassigned")`.
#[non_exhaustive]
pub enum RegisterWorkerOutcome {
    Registered,
    Refreshed,
}

/// Feature gate: `core`.
async fn register_worker(
    &self,
    args: RegisterWorkerArgs,
) -> Result<RegisterWorkerOutcome, EngineError> {
    Err(EngineError::Unavailable { op: "register_worker" })
}

/// Args for `heartbeat_worker`.
#[non_exhaustive]
pub struct HeartbeatWorkerArgs {
    pub worker_instance_id: WorkerInstanceId,
    pub namespace: Namespace,
    pub now: TimestampMs,
}

#[non_exhaustive]
pub enum HeartbeatWorkerOutcome {
    /// TTL refreshed. `next_expiry_ms = now + last-registered ttl`
    /// — stored at register-time, not re-supplied by caller.
    /// Callers schedule the next heartbeat from this value.
    Refreshed { next_expiry_ms: TimestampMs },
    /// Liveness key was absent — TTL ran out or `mark_worker_dead`
    /// landed earlier. Caller re-registers, not re-heartbeats.
    NotRegistered,
}

/// Feature gate: `core`.
async fn heartbeat_worker(
    &self,
    args: HeartbeatWorkerArgs,
) -> Result<HeartbeatWorkerOutcome, EngineError> {
    Err(EngineError::Unavailable { op: "heartbeat_worker" })
}

/// Args for `mark_worker_dead`. Explicit operator action.
///
/// `reason` is capped at 256 bytes and must not contain control
/// characters; oversize / invalid reject with
/// `Validation(InvalidInput, "reason: …")`. Mirrors
/// `fail_execution`'s `failure_reason` discipline.
#[non_exhaustive]
pub struct MarkWorkerDeadArgs {
    pub worker_instance_id: WorkerInstanceId,
    pub namespace: Namespace,
    pub reason: String,
    pub now: TimestampMs,
}

#[non_exhaustive]
pub enum MarkWorkerDeadOutcome {
    Marked,
    /// Liveness key already absent — no-op, idempotent. Unified
    /// variant name with `HeartbeatWorkerOutcome::NotRegistered`
    /// for cross-method consistency.
    NotRegistered,
}

/// Feature gate: `core`.
async fn mark_worker_dead(
    &self,
    args: MarkWorkerDeadArgs,
) -> Result<MarkWorkerDeadOutcome, EngineError> {
    Err(EngineError::Unavailable { op: "mark_worker_dead" })
}

/// Cursor for `list_expired_leases`. Tuple (not bare `ExecutionId`)
/// so pagination is stable under equal-expiry: ZRANGEBYSCORE with
/// `LIMIT` keyed on score alone duplicates or skips when two leases
/// share a millisecond. Backends order strictly by
/// `(expires_at_ms ASC, execution_id ASC)`.
///
/// Example: `ExpiredLeasesCursor { expires_at_ms: TimestampMs::new(1_715_856_000_000), execution_id: ExecutionId::parse("{fp:7}:11111111-2222-3333-4444-555555555555").unwrap() }`.
#[non_exhaustive]
pub struct ExpiredLeasesCursor {
    pub expires_at_ms: TimestampMs,
    pub execution_id: ExecutionId,
}

/// Args for `list_expired_leases`.
///
/// Per-call partition fan-out capped at `PARTITION_SCAN_CHUNK`
/// (32, matching unblock scanner's rolling-window convention) so
/// latency stays bounded on Valkey. Callers sweep full keyspace
/// across iterations via `after`, not per-call.
#[non_exhaustive]
pub struct ListExpiredLeasesArgs {
    /// Every returned lease has `expires_at_ms <= as_of`. Caller's
    /// `now_ms` on hot paths; pinned-past in tests.
    pub as_of: TimestampMs,
    /// Exclusive pagination cursor. `None` = scan from earliest.
    pub after: Option<ExpiredLeasesCursor>,
    /// Default 1000 when `None`; backend cap 10_000 for bulk scans.
    pub limit: Option<u32>,
    /// Max partitions to fan across per call. Default 32 when `None`.
    pub max_partitions_per_call: Option<u32>,
    /// `None` = cross-namespace sweep for operator tooling (auth
    /// enforced at ff-server admin route, NOT the trait boundary).
    /// `Some(ns)` = per-tenant scope, cairn's default.
    pub namespace: Option<Namespace>,
}

#[non_exhaustive]
pub struct ExpiredLeaseInfo {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub worker_instance_id: WorkerInstanceId,
    pub expires_at_ms: TimestampMs,
    pub attempt_index: AttemptIndex,
}

#[non_exhaustive]
pub struct ListExpiredLeasesResult {
    pub entries: Vec<ExpiredLeaseInfo>,
    pub cursor: Option<ExpiredLeasesCursor>,
}

/// Feature gate: `suspension` (reads lease/attempt state).
async fn list_expired_leases(
    &self,
    args: ListExpiredLeasesArgs,
) -> Result<ListExpiredLeasesResult, EngineError> {
    Err(EngineError::Unavailable { op: "list_expired_leases" })
}
```

**Default impls return `EngineError::Unavailable`** so out-of-tree backends keep compiling. Every in-tree backend overrides. Paired with five RFC-018 `Supports.*` flags: `register_worker`, `heartbeat_worker`, `mark_worker_dead`, `list_expired_leases`, `list_workers` (Phase 6 add, §9.4) — all `true` on every in-tree impl at RFC completion.

## 5. Per-backend delivery

### 5.1 Valkey

`register_worker` lands as a new atomic Lua FCALL; heartbeat + mark_dead + list_expired stay as direct commands (each atomic on its own). `FLOWFABRIC_LIB_VERSION` bumps.

| Method | Impl |
|---|---|
All keys are namespace-prefixed (locked §9.1): `ff:worker:{ns}:{inst}:alive`, `ff:worker:{ns}:{inst}:caps`, `ff:idx:{ns}:workers`. Two namespaces reusing the same `worker_instance_id` can no longer collide at the key layer.

| `register_worker` | New FCALL `ff_register_worker`, KEYS=3 (`alive`, `caps_hash`, `idx_workers`), ARGV=(worker_id, lanes_csv, caps_csv, ttl_ms, now). Body: SET PX alive (no NX — idempotent overwrite per §9.3) + HSET caps_hash {worker_id, lanes_csv, caps_csv, ttl_ms} + SADD idx_workers. Atomic; concurrent mark_worker_dead can't interleave. Returns `"registered"` (prior alive missing) or `"refreshed"` (prior alive present; caps/lanes/TTL all overwritten). |
| `heartbeat_worker` | HGET `ff:worker:{ns}:{inst}:caps` ttl_ms (single round-trip) → PEXPIRE `ff:worker:{ns}:{inst}:alive <ttl>`. `0` reply → `NotRegistered`. Refreshed branch computes `next_expiry_ms = now + ttl`. |
| `mark_worker_dead` | MULTI/EXEC: DEL alive + DEL caps + SREM `ff:idx:{ns}:workers`. Atomic via MULTI. reason-string validation at trait ingress. |
| `list_expired_leases` | `ZRANGEBYSCORE ff:idx:{p}:lease_expiry -inf <as_of> WITHSCORES LIMIT 0 <limit>` fanned across `max_partitions_per_call` partitions (default 32 per §9.2) starting from cursor's partition offset; merged + sorted by `(expires_at_ms, execution_id)`. Cursor is `ExpiredLeasesCursor`. |

**Effort:** ~200 LOC net — `ff_register_worker` Lua (~15 lines) + 4 Rust bodies + SDK preamble compat test.

### 5.2 Postgres

Migration 0021 lands two tables:

```sql
-- Current state (one row per live worker_instance_id).
CREATE TABLE ff_worker_registry (
    -- fnv1a_u64(worker_instance_id.as_bytes()) % 256 as smallint.
    -- Documented derivation rule — both register + heartbeat +
    -- mark_worker_dead compute the same partition for the same id.
    partition_key          smallint NOT NULL,
    namespace              text     NOT NULL,
    worker_instance_id     text     NOT NULL,
    worker_id              text     NOT NULL,
    -- lanes as text[] since a worker serves one-or-more lanes
    -- (Rev-3 fix). Stored sorted so equality-checks are stable.
    lanes                  text[]   NOT NULL,
    capabilities_csv       text     NOT NULL,
    last_heartbeat_ms      bigint   NOT NULL,
    liveness_ttl_ms        bigint   NOT NULL,
    registered_at_ms       bigint   NOT NULL,
    PRIMARY KEY (partition_key, namespace, worker_instance_id)
) PARTITION BY HASH (partition_key);
-- 256 hash partitions, mirrors ff_budget_usage_by_exec's shape.

-- Append-only audit trail. Shape supports future operator-tooling
-- (listing recently-dead workers, registration bursts) without a
-- 0022 migration churn. Unused by this RFC's bodies; written to
-- by mark_worker_dead + the TTL-sweep scanner.
CREATE TABLE ff_worker_registry_event (
    partition_key          smallint NOT NULL,
    namespace              text     NOT NULL,
    worker_instance_id     text     NOT NULL,
    -- 'registered' | 'heartbeat' | 'marked_dead' | 'ttl_swept'
    event_kind             text     NOT NULL,
    event_at_ms            bigint   NOT NULL,
    -- Free-form; populated from MarkWorkerDeadArgs.reason for
    -- mark_dead events, null otherwise.
    reason                 text     NULL,
    PRIMARY KEY (partition_key, namespace, worker_instance_id, event_at_ms)
) PARTITION BY HASH (partition_key);
```

Plus a new TTL-sweep scanner (§5.4 below).

`list_expired_leases` uses the existing `ff_attempt` + `ff_exec_core` join, keyed on a new expiry index:

```sql
CREATE INDEX ff_attempt_lease_expiry_idx
    ON ff_attempt (partition_key, lease_expires_at_ms)
    WHERE lease_id IS NOT NULL AND public_state IN ('claimed', 'running');
```

Partial index keeps the scan small (only live leases). Migration `0021_worker_registry.sql`.

### 5.3 SQLite

Mirror the PG schema, flat tables (no partitioning per RFC-023 §4.1 A3). Migration `0021_worker_registry.sql` sibling. `lanes` stored as text (sorted-joined CSV) since SQLite lacks text[]; encoding matches Valkey's CSV.

### 5.4 PG/SQLite TTL sweep scanner

New scanner `ff_worker_registry_ttl_sweep`, per-partition, 30s interval matching the existing `budget_reconciler` cadence. Body:

```sql
DELETE FROM ff_worker_registry
WHERE partition_key = $1
  AND last_heartbeat_ms + liveness_ttl_ms < $2;  -- $2 = now_ms
```

Each eviction appends a `'ttl_swept'` event to `ff_worker_registry_event`. Without this, rows persist past declared liveness and `heartbeat_worker` returns `Refreshed` on a logically-dead row. Valkey's native PEXPIRE handles this natively; PG/SQLite need the explicit sweep.

**Delivery phasing** (mirrors #453/#454):
- Phase 1: RFC accept + types/contracts in `ff_core::contracts`.
- Phase 2: Valkey bodies + SDK preamble refactor.
- Phase 3: PG bodies + migration 0021.
- Phase 4: SQLite bodies.
- Phase 5: cairn migration (consumer-side swap of their bespoke Valkey impl for the trait).
- Phase 6: `list_workers` trait method (§9.4) + capability-matrix / docs + release gate.

~5 sessions per phase cadence of 453/454 — **estimate ~8-10 sessions total**.

## 6. Non-goals

1. **Worker fencing at the SQL layer.** Concurrent register of the same `worker_instance_id` is rejected at FF trait ingress via `Validation(InvalidInput)`. On Valkey, `ff_register_worker`'s FCALL atomicity covers the SET+HSET+SADD race. On PG/SQLite, the `(partition_key, namespace, worker_instance_id)` PK + `ON CONFLICT DO UPDATE` path handles concurrent register idempotently.
2. ~~**Worker discovery / listing live workers.**~~ **PROMOTED** — `list_workers` folded into Phase 6 per §9.4. No longer a non-goal.
3. **Lease reclaim dispatch.** `list_expired_leases` returns data; the *decision* to reclaim (via `reclaim_execution`, RFC-024) stays with the caller. FF doesn't auto-reclaim behind the scenes.
4. **Cross-partition global worker identity.** `worker_instance_id` is unique within `(namespace, partition)`; two namespaces can reuse the same string. Matches current Valkey shape.
5. **Worker-to-lane fencing at the storage layer.** Nothing in this RFC prevents two workers advertising the same lane + same caps; scheduler-side admission (RFC-012 claim_for_worker) already handles that contention fairly. Adding fencing would duplicate existing logic.
6. **Per-worker backpressure signals.** No `current_in_flight_count` field on the registration row. Consumers that want that signal can maintain it out-of-band.
7. **Worker restart-crash-loop detection.** Heartbeat cadence + `last_heartbeat_ms` gives the raw data; detection logic belongs in operator tooling, not the trait.
8. **Cross-worker leader election.** Any orchestrator-level leader-election (e.g. for cron dispatching) is consumer-layer and orthogonal to this RFC.
9. **Worker-stats aggregation / operator-event stream.** Deferred to a future RFC alongside any operator event stream (likely the same RFC that introduces cross-worker dashboards).

## 7. Terminology glossary

Used throughout this RFC; added as a standing glossary for future consumer RFCs that need to distinguish.

- **`worker_id` (FF)** — pool / logical identity. Stable across restarts. Multiple worker processes (instances) can share the same `worker_id` (horizontal scale-out of a worker pool).
- **`worker_instance_id` (FF)** — process identity. Unique per-boot. Identifies the specific OS process a lease is held by.
- **`worker_id` (cairn upstream)** — maps to FF's `worker_instance_id`. Cairn's adapter layer performs the rename; FF's trait doesn't bend to the consumer's naming.

## 8. Consumer migration

Cairn's current Valkey-specific worker registry (`cairn-fabric::engine::valkey_impl`) drops once the trait ships:

```rust
// Before (cairn valkey_impl.rs today):
fn register_worker(...) {
    self.client.cmd("HSET").arg("ff:worker:...").execute().await?;
    self.client.cmd("SADD").arg("ff:workers:active").execute().await?;
}

// After (both Valkey + Postgres in cairn):
fn register_worker(...) {
    self.backend.register_worker(args).await
}
```

Cairn's PR-C4 stubs (`unimplemented!("PR-C4")`) resolve as the trait methods land on PG.

## Rev-2 changelog — technical lens findings

Nine load-bearing changes from Rev-1:

1. **`list_expired_leases` cursor is now `(expires_at_ms, ExecutionId)` tuple.** `ExecutionId` alone is unstable under equal expiry (cluster fanouts, worker-pool registration bursts). ZRANGEBYSCORE by score with a single-id cursor duplicates or skips. New type `ExpiredLeasesCursor { expires_at_ms: TimestampMs, execution_id: ExecutionId }`; backend contracts order strictly by `(expires_at_ms ASC, execution_id ASC)`.
2. **Per-call partition fan-out cap.** `list_expired_leases` on Valkey costs N_partitions ZRANGEBYSCORE round trips (256 in the default config). Add `ListExpiredLeasesArgs::max_partitions_per_call: Option<u32>` defaulting to `PARTITION_SCAN_CHUNK` (32, matching unblock scanner's rolling-window convention). Callers sweep the full space across iterations via `cursor`, not per-call. Bounds `list_expired_leases` latency at ~10ms per tick under default Valkey round-trip latency.
3. **Postgres schema derivation rule.** `partition_key` on `ff_worker_registry` derived as `(fnv1a_u64(worker_instance_id.as_bytes()) % 256) as smallint` — documented in migration 0021 SQL comment + `contracts::RegisterWorkerArgs` rustdoc. Guarantees cross-process reads hit the same partition for the same id.
4. **Postgres `list_expired_leases` index respecifies existing columns.** `ff_attempt` has `lease_id_current` + `lease_expires_at_ms`; `ff_exec_core` has `public_state`. Index is on `ff_attempt (partition_key, lease_expires_at_ms) WHERE lease_id_current IS NOT NULL`; the `public_state IN ('claimed', 'running')` gate lives in the body's JOIN with `ff_exec_core`, not the index predicate (public_state isn't on the attempt row).
5. **Outcome-naming harmony.** `HeartbeatWorkerOutcome::NotRegistered` + `MarkWorkerDeadOutcome::AlreadyAbsent` both meant "key absent, caller's concern is separate". Unified as `{Heartbeat,MarkWorkerDead}Outcome::NotRegistered` — same enum variant name across both methods.
6. **`last_registered_ttl_ms` stored on the registration.** Heartbeat refreshes TTL to the *last-registered* value, not a caller-supplied one. Valkey: stash under `HSET ff:worker:{inst}:caps ttl_ms`. PG/SQLite: `liveness_ttl_ms` column on `ff_worker_registry`.
7. **SDK preamble stays inline; trait methods are the orthogonal surface.** `ff_sdk::valkey_preamble::run` currently runs at `FlowFabricWorker::connect` BEFORE the backend arc is constructed — chicken-egg. Rev-2 resolves this by keeping the preamble's three Valkey-specific writes inline (SET NX PX alive + SET caps + SADD index) and having `register_worker`'s Valkey body emit the identical byte pattern. Preamble is the fast-path for in-process worker construction; the trait method is the cross-orchestrator surface. Both write the same keys. Cairn migrates to the trait; `FlowFabricWorker::connect` keeps the preamble for latency.
8. **Feature gates split by storage primitive.** `register_worker` / `heartbeat_worker` / `mark_worker_dead` go under `core` (pure worker-identity state, no lease coupling). `list_expired_leases` goes under `suspension` (reads lease/attempt state coupled with suspend). Matches RFC-012 gating discipline.
9. **`reason: String` is length-capped + validated.** Same 256-byte cap + control-char rejection as `fail_execution`'s `failure_reason`. Oversize rejects with `Validation(InvalidInput, "reason: exceeds 256 bytes")`. Rejected bytes never land in storage or the operator event stream.

## Rev-3 changelog — consumer lens findings

Eight changes on top of rev-2:

1. **`lane_id: LaneId` → `lanes: BTreeSet<LaneId>`** on `RegisterWorkerArgs`. Workers serve multiple lanes (`FlowFabricWorker::connect` takes `Vec<LaneId>`); single-lane forces N round-trips per worker. `BTreeSet` (not `Vec`) for stable iteration + dedup.
2. **Cairn's upstream `register_worker(worker_id, capabilities)` is narrower than FF's trait args.** Expected — FF's trait ships the full identity surface (worker_instance_id, lanes, liveness_ttl_ms, namespace, now) because those are load-bearing for correctness (passive TTL expiry, multi-tenant isolation). Cairn owns the narrowing adapter in their `Engine` trait; FF's `EngineBackend` does not bend to the consumer's current shape.
3. **`namespace: Option<Namespace>` on `ListExpiredLeasesArgs`.** `None` = cross-namespace sweep for operator tooling; documented requires auth (the trait boundary doesn't enforce — ff-server's admin route does). `Some(ns)` = per-tenant scoped list (cairn's default).
4. **`HeartbeatWorkerOutcome::Refreshed { next_expiry_ms }`** — was `last_heartbeat_ms` (echo of input). Caller needs `next_expiry_ms` to schedule the next heartbeat without separately re-deriving from ttl + now.
5. **Operator events deferred to a follow-up RFC.** Rev-1 / Rev-2 proposed emitting `WorkerDeathRecorded` through RFC-019 `LeaseEvent` stream, but that stream is execution-scoped; cross-coupling domains is a mistake. No `operator_events` infrastructure exists today. This RFC drops the operator-event emission entirely; `mark_worker_dead` just clears the registry rows + returns `Marked` / `NotRegistered`. A follow-up RFC (RFC-027 or later) can introduce an operator event stream covering worker liveness + other operator-scope events together.
6. **`ListExpiredLeasesArgs` cap raised 1000 → 10_000.** Cairn's reclaim scanner sweeps many-thousands per tick under degraded-worker scenarios. Cursor-based pagination still works; the higher cap reduces round-trip count in bulk-reclaim cases. Default `limit` (when `None`) is 1000, matching `ListPendingWaitpointsArgs`.
7. **`RegisterWorkerOutcome` semantics disambiguated.** `Registered` = no prior live key for this `worker_instance_id` (fresh boot or post-TTL-expiry). `Refreshed` = an existing live key was found and TTL/caps overwritten (in-process hot-restart). Lets callers log registration as a discrete event without tracking prior state.
8. **Headline example spec'd for §9 release gate.** `examples/worker-registry-roundtrip/` — registers 3 workers with distinct capability sets + 2 lanes each, heartbeats 2 of them, marks the third dead with a reason, waits for the 2nd to TTL-expire naturally, calls `list_expired_leases` and asserts only the TTL-expired lease appears. Runs against all three backends via `--backend {valkey,postgres,sqlite}` flag. Live-runs green under `scripts/run-all-examples.sh` phase 3b.

## Rev-4 changelog — framing / strategic lens findings

Eight changes on top of rev-3:

1. **RFC-018 Supports-flag discipline obeyed.** Four new bools added: `supports.register_worker`, `supports.heartbeat_worker`, `supports.mark_worker_dead`, `supports.list_expired_leases`. Each true on all three in-tree backends at RFC completion; false on the pre-RFC default impl. Out-of-tree backends that don't implement all four see the flags appropriately false, dispatch catches `EngineError::Unavailable` gracefully.
2. **Valkey `register_worker` gets a new Lua function `ff_register_worker`.** Rev-3's "no new Lua functions" was fragile — three-round-trip SET NX PX + SET + SADD isn't atomic, and a concurrent `mark_worker_dead` can interleave leaving a zombie `:caps` key without `:alive`. `ff_register_worker` does all three writes in one atomic FCALL (~15 lines of Lua, KEYS=3, ARGV=caps_csv + ttl_ms + now). `heartbeat_worker` (single PEXPIRE, already atomic) and `mark_worker_dead` (single MULTI/EXEC with DEL + SREM, already atomic via MULTI) stay as direct commands. `FLOWFABRIC_LIB_VERSION` bumps.
3. **Postgres schema split: state table + event table.** `ff_worker_registry` holds current state (one row per live `worker_instance_id`); new append-only `ff_worker_registry_event` captures register / heartbeat / dead transitions for future operator-tooling (listing recently-dead workers, audit trails). Event table lands in migration 0021 alongside the state table; unused by this RFC's bodies but shapes future work without a 0022 migration churn. SQLite mirrors, flat tables.
4. **PG/SQLite TTL sweep.** Valkey's TTL cleanup is native (PEXPIRE drops the key). PG/SQLite need a reconciler: new `ff_worker_registry_ttl_sweep` scanner (per-partition, 30s interval matching budget_reconciler cadence) that deletes `ff_worker_registry` rows where `last_heartbeat_ms + liveness_ttl_ms < now_ms AND state = 'alive'`. Without this, rows persist past declared liveness — semantic diverges from Valkey. Sweep logs each eviction at DEBUG with `(worker_instance_id, last_heartbeat_ms, liveness_ttl_ms)` so operators can see TTL-driven cleanup.
5. **Terminology block up front.** A glossary subsection (§3.5) defines FF's `worker_id` (pool/logical identity — stable across restarts, multiple instances share it) vs FF's `worker_instance_id` (process identity — unique per boot). Cairn's upstream `worker_id` maps to FF's `worker_instance_id`; the cairn adapter performs the rename. This is load-bearing because every consumer RFC and doc going forward can reference the glossary instead of re-deriving.
6. **Compat contract during rollout.** Phase 2 (Valkey body) and Phase 5 (cairn migration) are separated by Phase 3 (PG) + Phase 4 (SQLite). During that gap, **old SDK preamble + new trait-method writes MUST produce identical Valkey key bytes** — verified via a cross-crate integration test that calls `valkey_preamble::run` and `backend.register_worker(args)` with equivalent inputs and asserts `HGETALL ff:worker:{inst}:caps` + `SMEMBERS ff:idx:workers` return identical entries. Test added in Phase 2's PR.
7. **§Non-goals sweep.** Six additions: `list_workers` (see §F1 — acknowledged as known future addition, not principled deferral), `worker-to-lane fencing at the storage layer`, `per-worker backpressure signals`, `worker restart-crash-loop detection`, `cross-worker leader election`, `worker-stats aggregation` (future operator event stream). Each one-line with a pointer at what might motivate adding it later.
8. **§Risks expanded.** Three new entries: (a) ff-backend-postgres version pin during cairn rollout (schema drift hazard); (b) `list_expired_leases` performance under >10k live leases — explain-analyze required in the PR's §9 release gate; (c) TTL sweep scanner correctness — test-fixture pinned to fire the TTL-driven deletion AND the `mark_worker_dead` deletion, assert idempotency + ordering.

## 9. Open questions — RESOLVED 2026-05-02

All prior open items adjudicated by owner. No remaining blockers for accept.

Locked through rev-2/3/4:
- Expired-lease cursor type → `(expires_at_ms, ExecutionId)` tuple (rev-2).
- `list_expired_leases` cross-namespace → `namespace: Option<Namespace>` variant (rev-3).
- Operator-event payload → deferred to a future operator-event RFC (rev-3).

Locked 2026-05-02:

1. **Valkey worker-key namespace granularity** → **namespace-prefixed**. Keys shift to `ff:worker:{namespace}:{id}:alive` / `ff:worker:{namespace}:{id}:caps` / `ff:idx:{namespace}:workers`. Aligns with the PG PK which already includes `namespace`; makes multi-tenant instance_id collisions impossible. SDK preamble key-shape changes in Phase 2 (compat-test bytes verify FF trait writes equal preamble writes under the new shape).
2. **`ListExpiredLeasesArgs.max_partitions_per_call` default** → **32** (matches unblock-scanner convention, one round trip covers most sweeps).
3. **`ff_register_worker` on caps/lanes change under same `worker_instance_id`** → **Refreshed** (overwrite caps + lanes + TTL). Matches today's SDK preamble behaviour; supports hot-reloading workers with updated caps without an explicit mark_dead intermediate.
4. **`list_workers` operator-tooling** → **fold into RFC-025 Phase 6**. State table exists anyway; adding the read method is ~50 LOC + matrix row. Shape: `ListWorkersArgs { namespace: Option<Namespace>, after: Option<WorkerInstanceId>, limit: Option<u32> } -> ListWorkersResult { entries: Vec<WorkerInfo>, cursor: Option<WorkerInstanceId> }`. Pairs with a 5th RFC-018 `Supports.list_workers` flag.

## 10. Release gate / acceptance

- All four methods shipped on all three backends; no `Unavailable` in any in-tree impl.
- CLAUDE.md §5 full release gate (smoke, examples live-run via `scripts/run-all-examples.sh` phase 3e harness, new headline example for worker-registry round trip).
- `docs/POSTGRES_PARITY_MATRIX.md` gains four rows, all `impl` on all three backends.
- Cairn's `valkey_impl` / `postgres_control_plane_impl` both swap to trait dispatch in the same wave (cairn PR-C5).

## 11. Risks

- **Schema bump on PG/SQLite** — migration 0021 lands under sqlx check; no shape of `ff_exec_core` / `ff_attempt` changes.
- **SDK preamble refactor** — the one consumer-breaking shape is if the preamble's current `SET ff:worker:{id}:alive NX` semantics drift; we preserve NX (duplicate-instance guard) via the trait's `Validation(InvalidInput, "instance_id reassigned")`. No wire-level change.
- **Cairn coordination** — PR-C4 lands before FF v0.14 content delivery; cairn's bespoke impl keeps working under `Unavailable`-on-PG during the FF delivery window. Phase 5 of this RFC is the coordinated flip.

---

## Revisions

- **Rev 1 (2026-05-02):** initial draft.
- **Rev 2 (2026-05-02):** technical-lens challenger round — cursor stability, partition fan-out cap, PG schema column grounding, outcome naming harmony, TTL storage, SDK preamble compat, feature gate split, reason-string hardening.
- **Rev 3 (2026-05-02):** consumer-lens challenger round — multi-lane registration, cairn adapter layer, cross-namespace expired-lease sweeps, heartbeat return shape, operator-event deferral, pagination cap, outcome semantic disambiguation, §9 headline example spec.
- **Rev 4 (2026-05-02):** framing-lens challenger round — RFC-018 Supports-flag discipline, new `ff_register_worker` Lua FCALL for atomicity, PG schema split (state + event tables), PG/SQLite TTL sweep scanner, terminology glossary, rollout compat contract, §Non-goals expansion, §Risks expansion.
