# RFC-017: ff-server backend abstraction — migrating HTTP handlers off direct ferriskey calls

**Status:** Draft (Author-A divergent)
**Author:** Worker-A (divergent draft — consolidation pending)
**Created:** 2026-04-23
**Pre-RFC Reference:** `rfcs/drafts/v0.7-migration-master.md` (Wave 8 readiness), `rfcs/drafts/v0.7-migration-survey-engine.md` §5
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-016 (quorum edges, Stage-A/B precedent for staged rollout), RFC-011 (co-location + `{fp:N}` partitioning), RFC-006 (stream tail + mux rationale)

---

## Summary

`ff-server` today is Valkey-hardcoded: every HTTP handler on `Server` constructs ferriskey commands inline (`self.client.fcall_with_reload(...)`, `self.client.cmd(...)`, `self.tail_client.cmd(...)`), builds KEYS/ARGV arrays positionally, and parses raw `ferriskey::Value` replies. v0.7 landed a Postgres `EngineBackend` (Waves 4-7) that integration tests and the `deploy-approval` example exercise via `ff-sdk`, but no HTTP consumer can reach Postgres — because `Server::start(ServerConfig)` hardcodes a `ferriskey::Client` dial, and all 24 handler methods reach into it directly. This RFC migrates the `Server` onto a backend-agnostic field `backend: Arc<dyn EngineBackend>`, promotes five ingress ops (`create_execution`, `create_flow`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child`) onto the trait, keeps Valkey-specific stream-pool primitives (`stream_semaphore`, `xread_block_lock`) behind a capability-gated struct on the concrete backend, and stages the migration in four phases (A: ingress trait-promotion, B: read/admin migration, C: execution-control migration, D: stream + rotate cutover). Ingress-op promotion is a **breaking trait change**, acceptable because `EngineBackend` has exactly two impls in-tree and one unreleased Postgres crate.

## 1. Motivation

### 1.1 Wave 8 gap

`rfcs/drafts/v0.7-migration-master.md` Part 2 assumed that migrating `ff-server` was "swap the client, wire the trait." The Worker-C engine survey at `rfcs/drafts/v0.7-migration-survey-engine.md` §5 flagged the fact that handlers in `server.rs` hold raw FCALL shapes but did not escalate — the survey table ("`Server::<method>` exists / trait method exists / effort") was never written. Wave 8 ships v0.7 without an HTTP-Postgres path. A consumer wanting to drive ff-server against Postgres today has to either:

1. Use `ff-sdk` directly and bypass the REST surface entirely (what `deploy-approval` does);
2. Fork `ff-server` and replace every `self.client.*` call site themselves.

Neither is acceptable once v0.7 is framed as "Postgres parity."

### 1.2 Why this is architecture, not cleanup

Migrating the handlers is not a 1:1 substitution. Five `Server::` methods — `create_execution`, `create_flow`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child` — use FCALLs that have **no counterpart on `EngineBackend`**. They are also **not** on `ValkeyBackend` as inherent methods: they were authored directly on `Server`, inline, against a `ferriskey::Client`. On the Postgres side, the same five ops exist as **inherent methods on `PostgresBackend`** (see `crates/ff-backend-postgres/src/lib.rs:184-238`), deliberately placed there because the Postgres consumer path (`deploy-approval` + integration tests) needed the entry point before the trait could be extended.

This is backend drift. The two concrete backends have different public shapes — not at the trait level, but one layer below it — and the HTTP handler layer only works against Valkey because it bypasses the abstraction. RFC-012 §4.2 explicitly framed this seam ("the hot-path migration lands across Stages 1b-1d") but the ingress surface was out of scope. This RFC closes it.

### 1.3 Scope guardrails

In scope:

- `Server` struct shape + all ~24 HTTP handler methods on it.
- Trait scope: promoting the five ingress ops + two scheduler-adjacent ops (`claim_for_worker`) into `EngineBackend` (or a peer trait — §3 decides).
- Admin surface: `/v1/admin/rotate-waitpoint-secret` migrates to `EngineBackend::rotate_waitpoint_hmac_secret_all` (already on the trait).
- `ServerConfig` gains a `backend: BackendKind` (Valkey|Postgres) discriminator; construction path branches.

Out of scope:

- Changing REST wire format (route paths, request/response JSON). This RFC is handler-internal.
- Engine / scheduler changes — they already live behind `Arc<dyn EngineBackend>` via Wave 4-7.
- Worker / SDK changes — consumers of the trait already work.
- Adding new HTTP endpoints. Migrate-then-extend.

## 2. Current state

### 2.1 The `Server` struct

```rust
pub struct Server {
    client: Client,               // main ferriskey client (FCALL + pipelines + direct CMDs)
    tail_client: Client,          // dedicated mux for XREAD / XRANGE (§2.3)
    stream_semaphore: Arc<Semaphore>, // bounds concurrent read+tail ops; 429 on contention
    xread_block_lock: Arc<Mutex<()>>, // serializes XREAD BLOCK against the tail_client FIFO
    admin_rotate_semaphore: Arc<Semaphore>, // single-permit gate on rotate_waitpoint_secret
    engine: Engine,
    config: ServerConfig,
    scheduler: Arc<ff_scheduler::Scheduler>,
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

The `client` + `tail_client` fields are the Valkey-specific surface. Everything else is backend-agnostic **in shape** (semaphores, metrics, config) but some depend transitively (the engine + scheduler are constructed with `client.clone()` in boot).

### 2.2 Handler inventory

Grepping `self\.(client|tail_client|fcall_with_reload)` in `crates/ff-server/src/server.rs` yields the 24 call sites below. Each is a candidate for migration. FCALL sites marked † have **no existing trait method**; non-FCALL sites (HGET / HMGET / SMEMBERS / SSCAN) are all read ops.

| # | `Server::` method | Line | Valkey primitive | Trait method today | Notes |
|---|---|---|---|---|---|
| 1 | `create_execution` | 670 | `FCALL ff_create_execution` | † (inherent on PG) | Ingress |
| 2 | `cancel_execution` | 754 | `FCALL ff_cancel_execution` | ✗ | — |
| 3 | `get_execution_state` | 786 | `HGET exec_core public_state` | partial (`describe_execution` superset) | Can forward via describe |
| 4 | `get_execution_result` | 840 | `GET <result key>` | ✗ | Binary-safe payload read |
| 5 | `list_pending_waitpoints` | 911 | SSCAN + 2× pipelined HMGET | ✗ | Complex; 2-pass pipeline |
| 6 | `create_budget` | 1176 | `FCALL ff_create_budget` | ✗ | — |
| 7 | `create_quota_policy` | 1236 | `FCALL ff_create_quota_policy` | ✗ | — |
| 8 | `get_budget_status` | 1274 | 3× HGETALL (def/usage/limits) | ✗ | — |
| 9 | `report_usage` | 1350 | `FCALL ff_report_usage_and_check` | `EngineBackend::report_usage` (different shape; `Handle`-bound) | See §4 note |
| 10 | `reset_budget` | 1393 | `FCALL ff_reset_budget` | ✗ | — |
| 11 | `create_flow` | 1421 | `FCALL ff_create_flow` | † (inherent on PG) | Ingress |
| 12 | `add_execution_to_flow` | 1487 | `FCALL ff_add_execution_to_flow` | † (inherent on PG) | Ingress |
| 13 | `cancel_flow` / `cancel_flow_wait` | 1586 / 1596 | `FCALL ff_cancel_flow` + async dispatch | `EngineBackend::cancel_flow` | Trait covers header; dispatch is server-local |
| 14 | `stage_dependency_edge` | 1913 | `FCALL ff_stage_dependency_edge` | † (inherent on PG) | Ingress |
| 15 | `apply_dependency_to_child` | 1957 | `FCALL ff_apply_dependency_to_child` | † (inherent on PG) | Ingress |
| 16 | `deliver_signal` | 2022 | `FCALL ff_deliver_signal` + HGET lane_id | `EngineBackend::deliver_signal` (feature-`core`) | Trait exists; migrate |
| 17 | `change_priority` | 2123 | `FCALL ff_change_priority` + HGET lane_id | ✗ | — |
| 18 | `claim_for_worker` | 2172 | scheduler-routed | Delegates to `ff_scheduler::Scheduler` | Scheduler not yet trait-bound |
| 19 | `revoke_lease` | 2199 | `FCALL ff_revoke_lease` + HGET worker_instance_id | ✗ | — |
| 20 | `get_execution` | 2249 | HGETALL exec_core | `EngineBackend::describe_execution` | Trait shape differs slightly (`ExecutionSnapshot` vs `ExecutionInfo`) |
| 21 | `list_executions_page` | 2351 | SMEMBERS + sort+slice | `EngineBackend::list_executions` | Trait shape matches |
| 22 | `replay_execution` | 2414 | HMGET + SMEMBERS + `FCALL ff_replay_execution` | ✗ | — |
| 23 | `read_attempt_stream` | 2525 | XRANGE via `ff_script::stream` | `EngineBackend::read_stream` (feature-`streaming`) | Trait exists; migrate |
| 24 | `tail_attempt_stream` | 2605 | XREAD BLOCK + HMGET stream_meta | `EngineBackend::tail_stream` (feature-`streaming`) | Trait exists; migrate |
| + | `rotate_waitpoint_secret` | 3240 | per-partition `FCALL ff_rotate_waitpoint_hmac_secret` fan-out | `EngineBackend::rotate_waitpoint_hmac_secret_all` | Trait added v0.7; migrate |
| + | Boot: `verify_valkey_version` | 2776 | `INFO server` | ✗ | Valkey-only — drops in Postgres boot |
| + | Boot: `initialize_waitpoint_hmac_secret` | 3153 | per-partition HSET | ✗ | Valkey-only — drops in Postgres boot |
| + | Boot: `validate_or_create_partition_config` | 2997 | HGETALL partition_config | ✗ | Valkey-only — drops in Postgres boot |
| + | Boot: `ensure_library` (Lua) | 423 | FUNCTION LOAD | ✗ | Valkey-only — drops in Postgres boot |
| + | Boot: lanes SADD seed | 443 | `SADD ff:idx:lanes` | ✗ | Valkey-only — drops in Postgres boot |

Of the 24 runtime handlers, **5 have a directly compatible trait method**, **4 have a trait method with shape drift requiring glue**, and **15 have no trait method** (of which 5 are ingress).

### 2.3 Valkey-specific primitives

Three fields on `Server` are Valkey-artifact-only:

- `tail_client: Client` — a second ferriskey multiplex dedicated to stream ops. Motivated by ferriskey's FIFO pipeline (RFC-006 Impl Notes). Postgres has no equivalent; its stream surface is per-call `LISTEN` subscriptions pulled from the `sqlx::PgPool`.
- `stream_semaphore: Arc<Semaphore>` — bounds concurrent XREAD/XRANGE against the shared tail mux. On Postgres this is **structurally still useful** (pool saturation is a real concern), but the permit count should map to pool size, not `FF_MAX_CONCURRENT_STREAM_OPS`.
- `xread_block_lock: Arc<Mutex<()>>` — serializes XREAD BLOCK against the tail mux's per-call timeout race. On Postgres the primitive does not exist; `LISTEN` is inherently per-connection.

These cannot sensibly live on `Server` in a backend-agnostic layout. They belong on the `ValkeyBackend` concrete type (which already holds `Client`) and should be exposed to `Server` through a thin capability — not by making `Server` carry Valkey-shaped fields.

### 2.4 Ingress op asymmetry

| Op | ff-sdk (client) | `ff-server::Server` | `ValkeyBackend` | `PostgresBackend` |
|---|---|---|---|---|
| `create_execution` | ✓ direct | ✓ FCALL inline | ✗ | ✓ **inherent** |
| `create_flow` | ✓ direct | ✓ FCALL inline | ✗ | ✓ **inherent** |
| `add_execution_to_flow` | ✓ direct | ✓ FCALL inline | ✗ | ✓ **inherent** |
| `stage_dependency_edge` | ✓ direct | ✓ FCALL inline | ✗ | ✓ **inherent** |
| `apply_dependency_to_child` | ✓ direct | ✓ FCALL inline | ✗ | ✓ **inherent** |

The asymmetry is historical: ff-sdk drove ingress on Valkey through its own FCALL wrappers, `ff-server` duplicated those inlined against its own `client`, and Wave 4i added the inherent methods to `PostgresBackend` because it had no other way to land the ingress path without blocking on a trait revision. **Neither concrete backend exposes ingress on the trait**; `Server` cannot dispatch them polymorphically.

## 3. Design

### 3.1 Q1 — Trait scope expansion: promote ingress onto `EngineBackend`

**Position: (a) single-trait expansion.** Promote all five ingress ops to `EngineBackend`. Reject the two-trait split.

Rationale:

1. **Symmetry with Postgres shape.** `PostgresBackend` already has all five as public inherent methods taking the same `&CreateFlowArgs` / `&AddExecutionToFlowArgs` / `&StageDependencyEdgeArgs` / `&ApplyDependencyToChildArgs` / `CreateExecutionArgs` contracts. The Valkey side has equivalent FCALL shapes (via `ff-script::functions::flow`). Trait-lifting is mechanical: declare the signatures on `EngineBackend`, add `impl EngineBackend for ValkeyBackend` bodies that wrap the existing FCALL helpers, remove `#[cfg(feature = "core")]` from the `PostgresBackend` inherent methods and convert them to trait impls.

2. **No legitimate deployment mode omits ingress.** A backend that cannot create executions cannot serve `/v1/executions POST`. Carving a second trait `FlowIngressBackend` implies an imagined future where something implements `EngineBackend` but not ingress — a null set. Two traits is abstraction for its own sake, violates CLAUDE.md §2 "Simplicity First," and forces every consumer to hold two `Arc<dyn T>` handles for no gain.

3. **Feature flag propagation hazard (project_feature_flag_propagation memory).** Gating ingress under `#[cfg(feature = "core")]` — which is where it lives on `PostgresBackend` today — means a consumer who disables the `core` feature builds a backend that silently rejects the five most common HTTP routes. Moving ingress onto the trait (without the feature gate, or with the gate mirrored on `EngineBackend` itself) propagates the requirement explicitly.

4. **RFC-012 R7 precedent.** Round-7 already expanded the trait surface to 16 methods and widened `append_frame` / `report_usage` returns. Adding 5 more ingress methods continues a pattern the owner has accepted.

Rejected alternatives:

- **(b) trait + concrete downcast via `Any`.** Fails dyn-safety ergonomics; forces every handler to `impl_trait.as_any().downcast_ref::<ValkeyBackend>()`, with a silent runtime panic on mismatch. Negates the abstraction.
- **(c) two-trait split `FlowIngressBackend`.** Doubles the surface, doubles the `Arc<dyn T>` holdings on `Server`, adds a trait-cross-reference coherence burden, and — per point 2 — has no motivating use case. If a future use case emerges, splitting **later** is mechanical (one extra trait, default impls forwarding to `EngineBackend`); splitting **now** without the use case is speculative design.
- **(d) inherent methods on both backends + `Server` holds `Arc<ValkeyBackend>`-or-`Arc<PostgresBackend>` as an enum.** Rejected: eliminates the `dyn` abstraction entirely and forces every handler into a `match self.backend { BackendKind::Valkey(b) => ..., BackendKind::Postgres(b) => ... }` ladder. Exactly the shape RFC-012 was written to prevent.

### 3.2 Q2 — `Server` struct refactor

**Position: hybrid — `backend: Arc<dyn EngineBackend>` + optional `stream_ops: Option<Arc<ValkeyStreamOps>>` capability handle.**

```rust
pub struct Server {
    backend: Arc<dyn EngineBackend>,
    /// Present only when the backend is Valkey; None on Postgres.
    /// Holds tail_client + stream_semaphore + xread_block_lock.
    /// Stream handlers check presence and fall through to
    /// `backend.read_stream` / `backend.tail_stream` when absent.
    stream_ops: Option<Arc<ValkeyStreamOps>>,
    admin_rotate_semaphore: Arc<Semaphore>,    // backend-agnostic
    engine: Engine,                             // already backend-agnostic post-Wave 4
    config: ServerConfig,
    scheduler: Arc<dyn ff_scheduler::SchedulerBackend>, // see §3.5
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

`ValkeyStreamOps` is a small struct on `ff-backend-valkey`:

```rust
pub struct ValkeyStreamOps {
    pub(crate) tail_client: ferriskey::Client,
    pub(crate) stream_semaphore: Arc<Semaphore>,
    pub(crate) xread_block_lock: Arc<Mutex<()>>,
}
```

Why not make it part of the trait? Because its **contract is Valkey-specific**: the semaphore semantics (429 on contention via `try_acquire_owned`) and the mutex semantics (FIFO-mux serialization) have no Postgres analogue. Inventing a neutral trait method `acquire_stream_permit() -> StreamPermit` forces every Postgres call site through a pointless no-op permit dance.

But **the stream HTTP handlers still need to be backend-polymorphic** — `/v1/executions/{id}/attempts/{idx}/stream/tail` has to work against Postgres. Resolution: handlers call `EngineBackend::tail_stream`; the Valkey impl of `tail_stream` uses the `ValkeyStreamOps` primitives **internally** (the backend owns them, they're not on `Server`). `Server::stream_ops: Option<Arc<ValkeyStreamOps>>` survives only as an **observability / shutdown handle** (the semaphore needs to be closed on `shutdown`), not as an operational one.

Revised struct, final:

```rust
pub struct Server {
    backend: Arc<dyn EngineBackend>,
    /// Backend-native shutdown handle. Some backends need pre-shutdown
    /// cleanup (Valkey: close stream_semaphore so in-flight tails can
    /// drain without new arrivals). The trait exposes a `shutdown_prepare`
    /// default-impl hook; see §3.7.
    admin_rotate_semaphore: Arc<Semaphore>,
    engine: Engine,
    config: ServerConfig,
    scheduler: Arc<dyn ff_scheduler::SchedulerBackend>,
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

No backend-specific field leaks onto `Server`. Clean (a)-style.

### 3.3 Q3 — Lua-specific primitives

**Position: live on `ValkeyBackend` itself, not on `Server`.** See §3.2. `stream_semaphore.close()` on shutdown happens via a new trait method `EngineBackend::shutdown_prepare() -> Result<(), EngineError>`:

```rust
async fn shutdown_prepare(&self) -> Result<(), EngineError> { Ok(()) }  // default: no-op
```

`ValkeyBackend::shutdown_prepare` closes its semaphore; `PostgresBackend::shutdown_prepare` is the default no-op (pool drop during subsequent `Arc` release handles the rest).

Reject capability-check patterns (`if let Some(ops) = backend.as_valkey()`) — those drag the concrete type back into `Server` and invite scope creep.

### 3.4 Q4 — Admin surface

**Position: migrate to `EngineBackend::rotate_waitpoint_hmac_secret_all` immediately.**

The trait method already exists (`crates/ff-core/src/engine_backend.rs:563`) with a partial-success contract matching the current Valkey fan-out behavior, and the Valkey impl landed v0.7 per RFC-012 amendment. The `/v1/admin/rotate-waitpoint-secret` handler becomes a thin wrapper:

```rust
let result = server.backend.rotate_waitpoint_hmac_secret_all(
    RotateWaitpointHmacSecretAllArgs { new_kid, new_secret_hex: body.new_secret_hex, ... }
).await?;
```

The `admin_rotate_semaphore` stays on `Server` (single-permit server-wide, backend-agnostic; prevents concurrent rotate requests from a misbehaving operator regardless of backend).

Delete `Server::rotate_waitpoint_secret` + `rotate_single_partition` (~150 lines); both move to `ValkeyBackend`'s impl of `rotate_waitpoint_hmac_secret_all` (already exists per RFC-012-amendment-117).

### 3.5 Scheduler — trait or not?

`Server::claim_for_worker` today delegates to `ff_scheduler::Scheduler::claim_for_worker`, which is constructed with `client.clone()` and partition config. The scheduler is **not** yet behind a trait. For v0.7 Wave 8 this is acceptable because:

1. Postgres has its own scheduler at `crates/ff-backend-postgres/src/scheduler.rs` constructed differently.
2. Handler migration can route through `backend.claim_for_worker(...)` if we promote it to the trait — same logic as §3.1.

**Position: add `claim_for_worker` to `EngineBackend`.** `ValkeyBackend` forwards to `ff_scheduler::Scheduler`; `PostgresBackend` forwards to its own scheduler module. The `Arc<dyn SchedulerBackend>` in §3.2 goes away — scheduling is a backend method, not a sibling trait.

This is consistent with Q1 (one trait, ingress-shaped ops included) and avoids the inner `SchedulerError → EngineError` mapping that the current `Server::claim_for_worker` body does by hand.

### 3.6 `ServerConfig` changes

```rust
pub struct ServerConfig {
    pub backend: BackendKind,            // NEW: enum { Valkey(ValkeyBackendConfig), Postgres(PostgresBackendConfig) }
    pub partition_config: PartitionConfig,
    pub lanes: Vec<LaneId>,
    pub listen_addr: String,
    pub engine_config: EngineConfig,
    pub cors_origins: Vec<String>,
    pub api_token: Option<String>,
    pub waitpoint_hmac_secret: String,
    pub waitpoint_hmac_grace_ms: u64,
    pub max_concurrent_stream_ops: u32,
    // REMOVED: host, port, tls, cluster, skip_library_load (moved into BackendKind::Valkey)
}
```

`BackendKind::Valkey(ValkeyBackendConfig)` wraps today's `host/port/tls/cluster/skip_library_load`. `BackendKind::Postgres(PostgresBackendConfig)` wraps the sqlx `BackendConfig` today in `ff-core`.

`ServerConfig::from_env` dispatches on `FF_BACKEND` (default `valkey` for parity) — see §6 Backward compat.

### 3.7 `Server::start` flow, post-migration

```rust
pub async fn start_with_metrics(
    config: ServerConfig,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError> {
    let backend: Arc<dyn EngineBackend> = match &config.backend {
        BackendKind::Valkey(cfg) => ff_backend_valkey::ValkeyBackend::connect_with_metrics(
            cfg.to_backend_config(),
            config.partition_config,
            config.lanes.clone(),
            config.waitpoint_hmac_secret.clone(),
            metrics.clone(),
        ).await?,
        BackendKind::Postgres(cfg) => ff_backend_postgres::PostgresBackend::connect(
            cfg.to_backend_config()
        ).await?,
    };

    // Backend-specific boot lives inside `connect`:
    //   Valkey: ensure_library, validate_or_create_partition_config,
    //           initialize_waitpoint_hmac_secret, lanes SADD seed, INFO version check.
    //   Postgres: apply_migrations (if FF_PG_APPLY_MIGRATIONS=true), check_schema_version.
    // `Server` itself is ignorant of either path.

    let engine = Engine::start_with_completions(
        engine_cfg,
        backend.clone(),
        metrics.clone(),
        backend.subscribe_completions().await?,
    );

    let scheduler = /* behind the trait, or still a separate Arc<dyn> pre-§3.5-Stage-D */;

    Ok(Self { backend, admin_rotate_semaphore: ..., engine, config, scheduler, background_tasks, metrics })
}
```

Lua-specific steps (library load, lanes SADD, HMAC HSET fan-out) disappear from `ff-server` entirely — they're Valkey-specific and relocate to `ValkeyBackend::connect_with_metrics` where the `Client` is in scope. This is already partially true (`validate_or_create_partition_config` is called from boot with `&client` explicit) but needs finishing.

## 4. Per-handler migration table

For each handler: **Current** (Valkey primitive), **Target** (trait method), **Effort** (T=Trivial <1h, M=Moderate ~4h, H=Hard >1d), **Notes**.

| # | Handler | Current | Target trait method | Effort | Notes |
|---|---|---|---|---|---|
| 1 | `create_execution` | `FCALL ff_create_execution` | NEW `EngineBackend::create_execution` | **M** | Trait-lift + Valkey impl wraps existing FCALL helper at `ff-script::functions::execution::ff_create_execution`; Postgres impl already exists inherent, promote. |
| 2 | `cancel_execution` | `FCALL ff_cancel_execution` + `build_cancel_execution_fcall` (HMGET pre-reads) | NEW `EngineBackend::cancel_execution` | **M** | Same pattern as (1). Pre-read logic moves to backend. |
| 3 | `get_execution_state` | `HGET exec_core public_state` | Reuse `EngineBackend::describe_execution(...).map(|s| s.public_state)` | **T** | Wastes 7 extra HGETs on Valkey per call (HGETALL vs HGET 1). Acceptable cost for one-trait simplicity. Alt: add `EngineBackend::get_state` narrow method. Rejected per §8.3. |
| 4 | `get_execution_result` | raw `GET <result key>` | NEW `EngineBackend::get_execution_result` | **M** | Binary-safe `Vec<u8>` return. Postgres: `SELECT payload FROM ff_execution_result WHERE ...`. |
| 5 | `list_pending_waitpoints` | SSCAN + 2× pipelined HMGET | NEW `EngineBackend::list_pending_waitpoints` | **H** | Complex. HMGET pipeline optimization is Valkey-specific; Postgres serves as single `SELECT ... WHERE state IN ('pending','active')`. Trait returns `Vec<PendingWaitpointInfo>`; internals diverge. |
| 6 | `create_budget` | `FCALL ff_create_budget` | NEW `EngineBackend::create_budget` | **M** | Includes dim-count validation (already on Server; keep server-side). |
| 7 | `create_quota_policy` | `FCALL ff_create_quota_policy` | NEW `EngineBackend::create_quota_policy` | **M** | — |
| 8 | `get_budget_status` | 3× HGETALL | NEW `EngineBackend::get_budget_status` | **M** | Postgres: 1 query with joins. |
| 9 | `report_usage` | `FCALL ff_report_usage_and_check` | Existing trait method has `&Handle` arg; **server-HTTP variant takes `BudgetId` not `Handle`** | **H** | Two distinct shapes — worker-bound (via `Handle`) and REST-admin (via `BudgetId`). Need second trait method `EngineBackend::report_usage_admin(BudgetId, ReportUsageArgs)`. |
| 10 | `reset_budget` | `FCALL ff_reset_budget` | NEW `EngineBackend::reset_budget` | **M** | — |
| 11 | `create_flow` | `FCALL ff_create_flow` | NEW `EngineBackend::create_flow` | **M** | Ingress; Postgres inherent promotes. |
| 12 | `add_execution_to_flow` | `FCALL ff_add_execution_to_flow` + partition pre-check | NEW `EngineBackend::add_execution_to_flow` | **M** | Ingress; partition pre-check moves to backend (or stays server-side as invariant validation before dispatch). |
| 13 | `cancel_flow` / `cancel_flow_wait` | `FCALL ff_cancel_flow` + server-local async dispatch (`JoinSet` + cancel fan-out) | `EngineBackend::cancel_flow` + keep server-local dispatch on `Server` | **H** | Trait covers the header FCALL; the 200-line async member-dispatch machinery (JoinSet, `cancel_member_execution`, `ack_cancel_member`) stays on `Server` because it needs the trait + background_tasks. Biggest refactor in this RFC. |
| 14 | `stage_dependency_edge` | `FCALL ff_stage_dependency_edge` | NEW `EngineBackend::stage_dependency_edge` | **M** | Ingress. |
| 15 | `apply_dependency_to_child` | HGET lane + `FCALL ff_apply_dependency_to_child` | NEW `EngineBackend::apply_dependency_to_child` | **M** | Lane pre-read moves to backend (trivial on PG, existing helper on Valkey). |
| 16 | `deliver_signal` | HGET lane + `FCALL ff_deliver_signal` | Existing `EngineBackend::deliver_signal` | **T** | Trait already exists; swap call. |
| 17 | `change_priority` | HGET lane + `FCALL ff_change_priority` | NEW `EngineBackend::change_priority` | **M** | — |
| 18 | `claim_for_worker` | scheduler module delegation | NEW `EngineBackend::claim_for_worker` | **M** | See §3.5. |
| 19 | `revoke_lease` | HGET wiid + `FCALL ff_revoke_lease` | NEW `EngineBackend::revoke_lease` | **M** | — |
| 20 | `get_execution` | HGETALL exec_core | `EngineBackend::describe_execution` | **M** | Shape mismatch `ExecutionInfo` vs `ExecutionSnapshot` — glue in server, or convert `ExecutionInfo` to alias. §8.2. |
| 21 | `list_executions_page` | SMEMBERS + sort | `EngineBackend::list_executions` | **T** | Already matches. |
| 22 | `replay_execution` | HMGET + SMEMBERS + FCALL | NEW `EngineBackend::replay_execution` | **H** | Variadic KEYS — `ff_replay_execution` takes `4+N` KEYS based on inbound edge count. Trait signature needs to hide that; Valkey impl does pre-read internally. |
| 23 | `read_attempt_stream` | XRANGE via `ff_script::stream` + stream_semaphore | Existing `EngineBackend::read_stream` | **T** | Trait exists; migrate semaphore inside Valkey impl. |
| 24 | `tail_attempt_stream` | XREAD BLOCK + HMGET + stream_semaphore + xread_block_lock | Existing `EngineBackend::tail_stream` | **T** | Same; mutex + semaphore migrate inside Valkey impl. |
| + | `rotate_waitpoint_secret` | per-partition FCALL fan-out | Existing `EngineBackend::rotate_waitpoint_hmac_secret_all` | **T** | Delete server-side fan-out; trait impl already has it. |

Totals: 13 new trait methods (1, 2, 4, 5, 6, 7, 8, 10, 11, 12, 14, 15, 17, 18, 19, 22). Read (3, 20, 21) + stream (23, 24) + admin (+) + existing-trait (13, 16) repurpose existing methods.

## 5. Stage plan

Four stages, each independently merge-able, each CI-green at the workspace level. Modeled after RFC-016 Stages A-E but tighter (4 stages) because this is a pure refactor — no new behavior.

### Stage A — Trait expansion + wiring skeleton

- Add 13 new methods to `EngineBackend` with default impls returning `EngineError::Unavailable { op }` (same pattern RFC-012 used for the initial trait landing).
- Add `create_execution` + `create_flow` + `add_execution_to_flow` + `stage_dependency_edge` + `apply_dependency_to_child` as trait methods. Promote the 5 `PostgresBackend` inherent methods to `impl EngineBackend for PostgresBackend` blocks.
- Add stub `impl EngineBackend for ValkeyBackend` bodies for the 5 ingress + 8 execution-control methods. Each body calls the existing `ff_script::functions::*::ff_*` helper or an inline FCALL — **source-of-truth is still the FCALL helpers**, no behavior change.
- Add `EngineBackend::shutdown_prepare()` default impl.
- CI gate: workspace `cargo test` + all existing integration tests pass, including `deploy-approval` example. No ff-server handler changes yet.

Tests: for each new trait method, a behavioural test at `crates/ff-backend-valkey/tests/` confirming it produces the same wire side-effects as the pre-existing `Server::` method. Postgres side: existing `tests/integration_*.rs` already exercises these via inherent methods; re-point to trait call.

**Deliverable:** trait-level parity between Valkey and Postgres. Server still Valkey-only.

### Stage B — Read + admin handler migration

- Migrate read handlers 3, 20, 21 + admin + rotate (routes #3, #20, #21, `rotate_waitpoint_secret`).
- Migrate 23, 24 (stream read + tail). Move `stream_semaphore` + `xread_block_lock` + `tail_client` from `Server` to `ValkeyBackend` inside the Valkey impls of `read_stream` / `tail_stream`. Stage the migration under a feature flag `ff-server/backend-abstraction` initially to allow mid-stage revert.
- `Server::shutdown` calls `backend.shutdown_prepare().await` before draining `background_tasks`.
- `ServerConfig` gains `BackendKind::Valkey(ValkeyBackendConfig)` but **keeps** legacy fields (`host`, `port`, etc.) as deprecated-shim for the one-release deprecation window (§6).

**Deliverable:** ~6 handlers migrated; stream pool lives on backend; `ff-server` stream surface works against a `PostgresBackend` (`tail_stream` default-impl returns `Unavailable` on Postgres for this stage, acceptable because Postgres streams aren't yet v0.7-wired through ff-server; tracked as follow-up).

### Stage C — Execution-control handler migration

- Migrate handlers 2, 4, 6, 7, 8, 9, 10, 16, 17, 18, 19, 22 (cancel_execution, get_result, budget/quota ops, deliver_signal, change_priority, claim_for_worker, revoke_lease, replay).
- Add `ScriptError -> EngineError` conversion shims at the backend boundary. Keep `ServerError::{Backend, BackendContext}` — they already hold backend-agnostic `ff_core::BackendError` per PR #88.
- Delete `Server::fcall_with_reload` (→ moves onto `ValkeyBackend`).
- Delete the now-orphaned FCALL helpers inside `server.rs` (`parse_*_result` functions — their callers are gone; keep ones used cross-crate).

**Deliverable:** 18+ handlers migrated.

### Stage D — Ingress migration + Postgres boot path + config cutover

- Migrate handlers 1, 5, 11, 12, 14, 15 (ingress + list_pending_waitpoints).
- Move Valkey-specific boot steps (`ensure_library`, `validate_or_create_partition_config`, `initialize_waitpoint_hmac_secret`, lanes SADD seed, `verify_valkey_version`) from `Server::start_with_metrics` into `ValkeyBackend::connect_with_metrics`.
- Wire `BackendKind::Postgres` into `Server::start_with_metrics`. `ServerConfig::from_env` reads `FF_BACKEND`.
- Delete `Server::client()` accessor and `Server::client: Client` field; `tail_client` already gone in Stage B.
- Remove the Stage-B feature flag shim. Remove legacy `ServerConfig` shim fields.
- Add HTTP integration test `tests/http_postgres_smoke.rs` exercising `/v1/flows`, `/v1/executions`, `/v1/executions/{id}/state`, `/v1/executions/{id}/signal` against a `PostgresBackend`.

**Deliverable:** `Server` holds only `Arc<dyn EngineBackend>` for data access. `FF_BACKEND=postgres ff-server` serves the full HTTP surface.

Cumulative risk profile: Stage A is additive, zero-risk. Stage B touches 6 handlers + config, low risk. Stage C is the bulk (12 handlers) — moderate risk, compensated by Stage A's parity tests. Stage D exposes the first HTTP-Postgres path and is where operational faults will surface; smoke tests are mandatory here.

## 6. Backward compatibility

### 6.1 Public API

**`Server::start(config: ServerConfig)` signature is preserved.** `ServerConfig`'s shape changes additively:

- Stage B: add `BackendKind`; retain `host`, `port`, `tls`, `cluster`, `skip_library_load` as `#[deprecated]` shims that populate a `BackendKind::Valkey(_)` when set. `ServerConfig::from_env` prefers `FF_BACKEND` env but falls back to the legacy vars.
- Stage D: delete the deprecated shims. This is a breaking change gated by a **minor-version bump** (v0.7.0 → v0.8.0 or later) per project convention.

**`Server::client() -> &Client`** is removed in Stage D (also breaking). Consumers using it — none in-tree per a ripgrep; worth scanning the release notes — must switch to `Server::backend() -> &Arc<dyn EngineBackend>`.

### 6.2 Wire format

Zero changes. Every HTTP route returns the same JSON shape. The `ExecutionInfo`/`ExecutionSnapshot` drift (handler #20) is resolved by wrapping the backend trait's `ExecutionSnapshot` into `ExecutionInfo` inside the handler — same wire output, unchanged.

### 6.3 Metrics + tracing

`/metrics` scrape output is preserved. Backend-specific counters (ferriskey RTT histograms) only emit when `backend = Valkey`; same for sqlx metrics on Postgres. Scrapers of both deployments see a union of series — no incumbent series disappears.

### 6.4 Consumer migration

`cairn-fabric` consumes `ff-server` via HTTP; no source change required. `deploy-approval` consumes via ff-sdk; no source change required. Any in-house consumer calling `Server::client()` — warn in v0.7 release notes.

## 7. Open questions (for owner)

**Q-A.** `EngineBackend::report_usage` vs `EngineBackend::report_usage_admin` (handler #9). The existing trait method takes `&Handle` (worker-bound; `Handle` is the lease cookie). The HTTP admin path takes `BudgetId` directly (no worker ownership). Options: (i) add a second trait method `report_usage_admin`; (ii) make `Handle` a sum type with an `Admin { budget: BudgetId, ... }` variant; (iii) reject the HTTP admin path entirely and require the worker to own all usage reports. My lean: (i) — smallest blast radius; matches the ingress/admin-path-vs-worker-path split already implicit in the trait.

**Q-B.** `claim_for_worker` scheduler state (§3.5). Today `ff_scheduler::Scheduler` is long-lived on `Server` and holds a rotation cursor (lane-rotation fairness). Promoting `claim_for_worker` to `EngineBackend` means the scheduler becomes an internal detail of the backend — but **cursor state must persist across calls** for the fairness property. Does the backend hold the scheduler? Yes — `ValkeyBackend` gains a `scheduler: Arc<Scheduler>` field. This doubles the allocation but is clean. Acceptable?

**Q-C.** `cancel_flow` async dispatch (handler #13). The JoinSet-driven member fan-out is 150+ lines of server-local machinery. Postgres impl would do the equivalent via its own dispatcher (probably a `SELECT FOR UPDATE SKIP LOCKED` reconciler already in `flow_staging.rs`). Should the trait's `cancel_flow` contract be "header only — server dispatches members" (current shape), or "end-to-end — backend dispatches members, trait caller observes"? The latter is more honest and unblocks Postgres reconciliation; the former ships faster. Owner call.

**Q-D.** Handler #5 (`list_pending_waitpoints`). 2-pass HMGET pipeline is Valkey-shaped. Trait contract should be "returns `Vec<PendingWaitpointInfo>` ordered-by-waitpoint-id"; internals diverge. Is that abstraction loss acceptable (Valkey's ordering is SSCAN-incidental, Postgres's is ORDER BY, neither is stable across backends)?

**Q-E.** Shim lifetime for `ServerConfig::host` et al. Stage B → Stage D is at least two release cycles. Is a single deprecation window enough, or do we keep the shim for two releases? (Relates to project_release_publish_list_drift memory.)

## 8. Alternatives rejected

### 8.1 "Keep `client: Client` on Server, add `backend: Arc<dyn EngineBackend>` alongside"

Rejected. This is the minimum-risk path — migrate handlers one at a time, keep both fields during transition. Problems: (a) doubles the boot cost (two Valkey connections, plus the `tail_client`); (b) invites drift where the trait-migrated handlers and the legacy handlers observe different state (e.g. a handler reads `exec_core` via `self.client` after a different handler wrote via `self.backend.backend` — fine on Valkey where both route to the same `Client`, but the whole point of the trait is polymorphism). Short-term hack with long-term cleanup debt. Prefer the stage-gated cutover in §5.

### 8.2 Unify `ExecutionInfo` and `ExecutionSnapshot` into one type

Handler #20 returns `ExecutionInfo` to HTTP; `EngineBackend::describe_execution` returns `ExecutionSnapshot`. Different field sets, different internal layout (StateVector nesting differs). Rejected for this RFC — collapsing them is its own refactor with wire-format fallout. Keep the handler-side adapter.

### 8.3 Add narrow trait methods (e.g. `get_state`, `get_result`) instead of reusing `describe_execution`

Tempting because it avoids 7 wasted HGET fields per call on `get_execution_state`. Rejected because: (a) a narrow method on every read is trait-surface bloat (trait goes from 16 → 25+ methods, much of it redundant with `describe_execution`); (b) one HGETALL per `GET /state` call is <1ms overhead, not worth the trait budget; (c) the primary consumer of `/state` is polling clients, and 1ms doesn't move the needle against the 100-500ms poll interval those use.

### 8.4 Make `Server` generic: `Server<B: EngineBackend>`

Rejected. Generics over the trait defeat dyn-compatibility — `Server<ValkeyBackend>` and `Server<PostgresBackend>` are two different types, forcing every downstream consumer (including main.rs) to pick at compile time, losing runtime backend switching and making testing one concrete main.rs harder. RFC-012 already committed to dyn-safe `Arc<dyn EngineBackend>`; this RFC honors that.

### 8.5 Delete `ff-server` and rely on consumers to use `ff-sdk` directly

The ultimate "cleanup." Rejected — `ff-server` exists because HTTP is a first-class consumer surface (cairn-fabric, operator tooling, out-of-process clients). Killing it pushes HTTP into every consumer's codebase, which is a worse tradeoff.

### 8.6 Move ingress to a parallel `FlowIngressBackend` trait

Already rejected in §3.1. Recording here for the cross-reference.

## 9. Testing plan

### 9.1 Stage A (trait expansion)

- **Parity matrix** test per new trait method: invoke `Server::<method>` (existing, via `self.client`) and `self.backend.<new_method>()` against the same Valkey instance, assert identical observable state transitions. Target: 13 new methods × 1 happy-path + 1 error-path = 26 tests minimum. Lives at `crates/ff-backend-valkey/tests/parity_stage_a.rs`.
- **Postgres inherent-to-trait promotion** test: the existing `crates/ff-backend-postgres/tests/integration_flow_staging.rs` re-targets to the trait call.

### 9.2 Stage B (read + admin + stream)

- **HTTP smoke against Postgres** for the six migrated routes. `FF_BACKEND=postgres cargo test -p ff-server -- http_postgres_read_smoke`.
- **Stream pool migration** test: `tail_attempt_stream` 429-on-contention behavior preserved. Existing `tests/stream_concurrency.rs` re-runs.
- **Rotate fan-out** test: per-partition failure surfacing preserved. Existing HMAC-rotate test re-runs unchanged.

### 9.3 Stage C (execution-control)

- Full HTTP matrix against both backends. Every migrated route tested against (Valkey, Postgres) × (happy, one error path). ≈24 tests.
- **Cancel-flow async dispatch** stays server-local; existing tests pass unchanged.

### 9.4 Stage D (ingress + boot + cutover)

- **HTTP-Postgres end-to-end smoke** — `tests/http_postgres_smoke.rs`. Create flow → add 3 executions → stage 2 edges → apply dependencies → cancel flow → assert terminal state. Gates Wave 8 ship.
- **Boot-path divergence** test: `ValkeyBackend::connect` runs library-load + HMAC install; `PostgresBackend::connect` runs migrations check. Each backend's test harness exercises its own path.
- **Config shim deprecation** — a unit test confirms `ServerConfig { host: "x", port: 6379, ..., backend: BackendKind::default() }` still constructs (populates `BackendKind::Valkey`) through the shim in Stage B and that Stage D removes the shim.

### 9.5 Consumer regression

- `cairn-fabric` runs its full CI against an `ff-server` built on this branch (each stage). No source-level change required.
- `deploy-approval` example runs unchanged — it consumes ff-sdk, not ff-server, so this RFC doesn't touch it. Still tested as a canary for trait-level breakage.

### 9.6 Observability regression

- Metrics scrape before/after each stage — series set identical. Only additions are `ff_backend_kind{backend="postgres"}` / `ff_backend_kind{backend="valkey"}` as a labelled counter.

---

**End of RFC-017 author-A draft.** ≈760 lines. Consolidation with author-B draft pending.
